//! Пассивный наблюдатель AT-SPI: дерево доступности окон Qt без кода в Anki.
//!
//! Qt публикует виджеты на шине доступности при
//! `QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1`. Наблюдатель только читает свойства
//! (роль, имя, состояния, координаты, текст) и ничего не вызывает в Anki,
//! поэтому видит и окна, которые показываются до загрузки аддонов.
//!
//! Работа соединения zbus выполняется на вызывающем потоке внутри
//! [`zbus::block_on`], поэтому каждый вызов синхронно ждёт ответа сам.

use crate::util::Result;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::time::Duration;
use zbus::connection::Builder;
use zbus::wire::DynamicDeserialize;
use zbus::{Connection, OwnedObjectPath, OwnedVariant, Type};

const REGISTRY: &str = "org.a11y.atspi.Registry";
const ROOT: &str = "/org/a11y/atspi/accessible/root";
const ACCESSIBLE: &str = "org.a11y.atspi.Accessible";
const NODE_BUDGET: usize = 400;
const MAX_DEPTH: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn center(self) -> (i32, i32) {
        (self.x + self.w / 2, self.y + self.h / 2)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Node {
    pub role: String,
    pub name: String,
    pub states: Vec<&'static str>,
    pub rect: Option<Rect>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    pub child_count: usize,
    pub children: Vec<Node>,
}

impl Node {
    pub fn has(&self, state: &str) -> bool {
        self.states.contains(&state)
    }

    pub fn walk<'a>(&'a self, out: &mut Vec<&'a Node>) {
        out.push(self);
        for c in &self.children {
            c.walk(out);
        }
    }
}

#[derive(Debug, Clone)]
pub struct TopLevel {
    /// Стабильная на время жизни виджета идентичность: шина и путь объекта.
    pub key: String,
    pub node: Node,
}

const STATE_NAMES: &[(u32, &str)] = &[
    (1, "active"),
    (4, "checked"),
    (8, "enabled"),
    (12, "focused"),
    (16, "modal"),
    (23, "selected"),
    (24, "sensitive"),
    (25, "showing"),
    (30, "visible"),
    (39, "is_default"),
];

pub fn decode_states(words: &[u32]) -> Vec<&'static str> {
    STATE_NAMES
        .iter()
        .filter(|(bit, _)| {
            let (word, offset) = ((bit / 32) as usize, bit % 32);
            words.get(word).is_some_and(|w| w & (1 << offset) != 0)
        })
        .map(|(_, n)| *n)
        .collect()
}

pub struct Atspi {
    bus: Connection,
}

fn err(e: impl std::fmt::Display) -> String {
    format!("AT-SPI: {e}")
}

impl Atspi {
    /// Адрес шины доступности выдаёт `org.a11y.Bus` сеансовой шины (D-Bus activation).
    pub fn connect(session_address: &str) -> Result<Atspi> {
        zbus::block_on(async {
            let session = Builder::address(session_address)
                .method_timeout(Duration::from_secs(10))
                .build()
                .await
                .map_err(err)?;
            let reply = session
                .call_method(
                    Some("org.a11y.Bus"),
                    "/org/a11y/bus",
                    Some("org.a11y.Bus"),
                    "GetAddress",
                    &(),
                )
                .await
                .map_err(err)?;
            let address: String = reply.body().deserialize().map_err(err)?;
            let bus = Builder::address(address.as_str())
                .method_timeout(Duration::from_secs(3))
                .build()
                .await
                .map_err(err)?;
            Ok(Atspi { bus })
        })
    }

    fn call<R>(
        &self,
        dest: &str,
        path: &str,
        iface: &str,
        method: &str,
        args: &(impl serde::Serialize + Type),
    ) -> Result<R>
    where
        R: DeserializeOwned + Type + for<'d> DynamicDeserialize<'d>,
    {
        let reply =
            zbus::block_on(
                self.bus
                    .call_method(Some(dest), path, Some(iface), method, args),
            )
            .map_err(err)?;
        reply.body().deserialize::<R>().map_err(err)
    }

    fn property(&self, dest: &str, path: &str, iface: &str, name: &str) -> Result<OwnedVariant> {
        self.call(
            dest,
            path,
            "org.freedesktop.DBus.Properties",
            "Get",
            &(iface, name),
        )
    }

    fn children(&self, dest: &str, path: &str) -> Result<Vec<(String, OwnedObjectPath)>> {
        self.call(dest, path, ACCESSIBLE, "GetChildren", &())
    }

    fn name(&self, dest: &str, path: &str) -> String {
        self.property(dest, path, ACCESSIBLE, "Name")
            .ok()
            .and_then(|v| String::try_from(v).ok())
            .unwrap_or_default()
    }

    /// Окна верхнего уровня всех приложений с указанным именем.
    pub fn top_levels(&self, application: &str) -> Result<Vec<TopLevel>> {
        let mut out = Vec::new();
        for (bus, path) in self.children(REGISTRY, ROOT)? {
            // Ошибка имени приложения не доказывает, что его окна исчезли.
            let name = self.property(&bus, path.as_str(), ACCESSIBLE, "Name")?;
            let name = String::try_from(name).map_err(err)?;
            if name != application {
                continue;
            }
            let windows = self.children(&bus, path.as_str())?;
            for (wbus, wpath) in windows {
                let mut budget = NODE_BUDGET;
                let node = self.node(&wbus, wpath.as_str(), 0, &mut budget)?;
                if node.has("showing") {
                    out.push(TopLevel {
                        key: format!("{wbus}{}", wpath.as_str()),
                        node,
                    });
                }
            }
        }
        Ok(out)
    }

    fn node(&self, dest: &str, path: &str, depth: usize, budget: &mut usize) -> Result<Node> {
        *budget = budget.saturating_sub(1);
        let role: String = self.call(dest, path, ACCESSIBLE, "GetRoleName", &())?;
        let states =
            decode_states(&self.call::<Vec<u32>>(dest, path, ACCESSIBLE, "GetState", &())?);
        let name = self.name(dest, path);
        let rect = self
            .call::<(i32, i32, i32, i32)>(
                dest,
                path,
                "org.a11y.atspi.Component",
                "GetExtents",
                &(0u32,),
            )
            .ok()
            .map(|(x, y, w, h)| Rect { x, y, w, h });
        let text = if name.is_empty()
            && matches!(
                role.as_str(),
                "label" | "text" | "paragraph" | "static" | "heading"
            ) {
            let count = self
                .property(dest, path, "org.a11y.atspi.Text", "CharacterCount")
                .ok()
                .and_then(|v| i32::try_from(v).ok())
                .unwrap_or(0);
            (count > 0)
                .then(|| {
                    self.call::<String>(
                        dest,
                        path,
                        "org.a11y.atspi.Text",
                        "GetText",
                        &(0i32, count.min(4000)),
                    )
                    .ok()
                })
                .flatten()
        } else {
            None
        };
        let mut node = Node {
            role,
            name,
            states,
            rect,
            text,
            child_count: 0,
            children: Vec::new(),
        };
        // Содержимое главного окна и webview не относится к диалогам: только заголовок.
        let opaque = (depth == 0 && node.role == "frame")
            || matches!(
                node.role.as_str(),
                "document web" | "html container" | "embedded" | "page tab list"
            );
        let children = if opaque {
            Vec::new()
        } else {
            self.children(dest, path).unwrap_or_default()
        };
        node.child_count = children.len();
        let list = matches!(
            node.role.as_str(),
            "list" | "list box" | "tree" | "table" | "tree table"
        );
        for (cbus, cpath) in children {
            if *budget == 0 || depth >= MAX_DEPTH {
                break;
            }
            append_child(
                &mut node,
                self.node(&cbus, cpath.as_str(), depth + 1, budget),
                list,
            );
        }
        Ok(node)
    }
}

fn append_child(parent: &mut Node, child: Result<Node>, list: bool) {
    let Ok(child) = child else {
        return;
    };
    // Из длинных списков сохраняем только выбранные и сфокусированные элементы.
    if list && !(child.has("selected") || child.has("focused")) {
        return;
    }
    parent.children.push(child);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_bits_cover_both_words() {
        let words = [(1 << 16) | (1 << 25) | (1 << 8), 1 << (39 - 32)];
        assert_eq!(
            decode_states(&words),
            vec!["enabled", "modal", "showing", "is_default"]
        );
        assert!(decode_states(&[]).is_empty());
    }

    #[test]
    fn inaccessible_child_does_not_discard_accessible_siblings() {
        let mut parent = Node {
            role: "frame".into(),
            name: "window".into(),
            states: vec!["showing"],
            rect: None,
            text: None,
            child_count: 2,
            children: Vec::new(),
        };
        append_child(&mut parent, Err("transient AT-SPI error".into()), false);
        append_child(
            &mut parent,
            Ok(Node {
                role: "dialog".into(),
                name: "available".into(),
                states: vec!["showing"],
                rect: None,
                text: None,
                child_count: 0,
                children: Vec::new(),
            }),
            false,
        );

        assert_eq!(parent.children.len(), 1);
        assert_eq!(parent.children[0].name, "available");
    }
}
