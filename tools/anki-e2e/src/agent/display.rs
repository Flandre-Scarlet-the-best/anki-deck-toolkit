//! Виртуальный дисплей X11: скриншоты, свойства окон и ввод через XTEST.
//!
//! Скриншот снимается с корневого окна настоящего Xvfb, а не рендерится
//! виджетом. Клики и клавиши — синтетические события сервера X, которые Qt
//! обрабатывает так же, как действия пользователя.

use crate::util::Result;
use serde::Serialize;
use std::path::Path;
use x11rb::CURRENT_TIME;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, ConnectionExt, ImageFormat,
    KEY_PRESS_EVENT, KEY_RELEASE_EVENT, MOTION_NOTIFY_EVENT, MapState, Window,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

pub struct Display {
    conn: RustConnection,
    root: Window,
    width: u16,
    height: u16,
}

#[derive(Debug, Clone, Serialize)]
pub struct X11Window {
    pub id: u32,
    pub title: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub override_redirect: bool,
    pub modal: bool,
    pub transient_for: Option<u32>,
    pub window_types: Vec<String>,
    pub class: Vec<String>,
}

fn err(e: impl std::fmt::Display) -> String {
    format!("X11: {e}")
}

impl Display {
    pub fn connect(name: &str) -> Result<Display> {
        let (conn, screen) = x11rb::connect(Some(name)).map_err(err)?;
        let s = &conn.setup().roots[screen];
        let (root, width, height) = (s.root, s.width_in_pixels, s.height_in_pixels);
        conn.xtest_get_version(2, 2)
            .map_err(err)?
            .reply()
            .map_err(err)?;
        Ok(Display {
            conn,
            root,
            width,
            height,
        })
    }

    fn atom(&self, name: &str) -> Result<u32> {
        Ok(self
            .conn
            .intern_atom(false, name.as_bytes())
            .map_err(err)?
            .reply()
            .map_err(err)?
            .atom)
    }

    fn atom_name(&self, atom: u32) -> String {
        self.conn
            .get_atom_name(atom)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| String::from_utf8_lossy(&r.name).into_owned())
            .unwrap_or_default()
    }

    fn property(&self, window: Window, name: &str, kind: u32) -> Option<Vec<u8>> {
        let atom = self.atom(name).ok()?;
        let reply = self
            .conn
            .get_property(false, window, atom, kind, 0, 4096)
            .ok()?
            .reply()
            .ok()?;
        (reply.format != 0).then_some(reply.value)
    }

    fn atoms(&self, window: Window, name: &str) -> Vec<String> {
        self.property(window, name, AtomEnum::ATOM.into())
            .map(|v| {
                v.as_chunks::<4>()
                    .0
                    .iter()
                    .map(|c| self.atom_name(u32::from_ne_bytes(*c)))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Видимые окна верхнего уровня (без WM они — прямые потомки root).
    pub fn windows(&self) -> Result<Vec<X11Window>> {
        let tree = self
            .conn
            .query_tree(self.root)
            .map_err(err)?
            .reply()
            .map_err(err)?;
        let utf8 = self.atom("UTF8_STRING")?;
        let mut out = Vec::new();
        for window in tree.children {
            let Ok(attrs) = self
                .conn
                .get_window_attributes(window)
                .map_err(err)?
                .reply()
            else {
                continue;
            };
            if attrs.map_state != MapState::VIEWABLE {
                continue;
            }
            let Ok(geometry) = self.conn.get_geometry(window).map_err(err)?.reply() else {
                continue;
            };
            let title = self
                .property(window, "_NET_WM_NAME", utf8)
                .or_else(|| self.property(window, "WM_NAME", AtomEnum::STRING.into()))
                .map(|v| String::from_utf8_lossy(&v).into_owned())
                .unwrap_or_default();
            let transient_for = self
                .property(window, "WM_TRANSIENT_FOR", AtomEnum::WINDOW.into())
                .filter(|v| v.len() >= 4)
                .map(|v| u32::from_ne_bytes([v[0], v[1], v[2], v[3]]));
            let class = self
                .property(window, "WM_CLASS", AtomEnum::STRING.into())
                .map(|v| {
                    v.split(|b| *b == 0)
                        .filter(|s| !s.is_empty())
                        .map(|s| String::from_utf8_lossy(s).into_owned())
                        .collect()
                })
                .unwrap_or_default();
            let state = self.atoms(window, "_NET_WM_STATE");
            out.push(X11Window {
                id: window,
                title,
                x: i32::from(geometry.x),
                y: i32::from(geometry.y),
                width: u32::from(geometry.width),
                height: u32::from(geometry.height),
                override_redirect: attrs.override_redirect,
                modal: state.iter().any(|s| s == "_NET_WM_STATE_MODAL"),
                transient_for,
                window_types: self.atoms(window, "_NET_WM_WINDOW_TYPE"),
                class,
            });
        }
        Ok(out)
    }

    /// PNG всего виртуального экрана (ZPixmap 24/32 бит, порядок BGRX).
    pub fn screenshot(&self, path: &Path) -> Result<()> {
        let image = self
            .conn
            .get_image(
                ImageFormat::Z_PIXMAP,
                self.root,
                0,
                0,
                self.width,
                self.height,
                !0,
            )
            .map_err(err)?
            .reply()
            .map_err(err)?;
        let pixels = usize::from(self.width) * usize::from(self.height);
        if image.data.len() < pixels * 4 {
            return Err(format!(
                "X11: неожиданный формат изображения ({} байт)",
                image.data.len()
            ));
        }
        let mut rgb = Vec::with_capacity(pixels * 3);
        for px in image.data.as_chunks::<4>().0.iter().take(pixels) {
            rgb.extend_from_slice(&[px[2], px[1], px[0]]);
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(err)?;
        }
        let file = std::fs::File::create(path).map_err(err)?;
        let mut encoder = png::Encoder::new(
            std::io::BufWriter::new(file),
            u32::from(self.width),
            u32::from(self.height),
        );
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(err)?;
        writer.write_image_data(&rgb).map_err(err)?;
        writer.finish().map_err(err)
    }

    pub fn click(&self, x: i32, y: i32) -> Result<()> {
        let (x, y) = (
            i16::try_from(x).map_err(err)?,
            i16::try_from(y).map_err(err)?,
        );
        self.conn
            .xtest_fake_input(MOTION_NOTIFY_EVENT, 0, CURRENT_TIME, self.root, x, y, 0)
            .map_err(err)?;
        self.conn.flush().map_err(err)?;
        std::thread::sleep(std::time::Duration::from_millis(120));
        for kind in [BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT] {
            self.conn
                .xtest_fake_input(kind, 1, CURRENT_TIME, self.root, 0, 0, 0)
                .map_err(err)?;
            self.conn.flush().map_err(err)?;
            std::thread::sleep(std::time::Duration::from_millis(60));
        }
        self.conn.sync().map_err(err)
    }

    fn keycode(&self, keysym: u32) -> Result<u8> {
        let setup = self.conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let map = self
            .conn
            .get_keyboard_mapping(min, max - min + 1)
            .map_err(err)?
            .reply()
            .map_err(err)?;
        let per = usize::from(map.keysyms_per_keycode);
        map.keysyms
            .chunks(per.max(1))
            .position(|syms| syms.contains(&keysym))
            .map(|i| min + u8::try_from(i).unwrap_or(0))
            .ok_or_else(|| format!("X11: нет keycode для keysym {keysym:#x}"))
    }

    /// Нажатие сочетания: модификаторы удерживаются, последняя клавиша нажимается.
    pub fn key_combo(&self, keysyms: &[u32]) -> Result<()> {
        let codes: Vec<u8> = keysyms
            .iter()
            .map(|k| self.keycode(*k))
            .collect::<Result<_>>()?;
        for code in &codes {
            self.conn
                .xtest_fake_input(KEY_PRESS_EVENT, *code, CURRENT_TIME, self.root, 0, 0, 0)
                .map_err(err)?;
            self.conn.flush().map_err(err)?;
            std::thread::sleep(std::time::Duration::from_millis(40));
        }
        for code in codes.iter().rev() {
            self.conn
                .xtest_fake_input(KEY_RELEASE_EVENT, *code, CURRENT_TIME, self.root, 0, 0, 0)
                .map_err(err)?;
            self.conn.flush().map_err(err)?;
            std::thread::sleep(std::time::Duration::from_millis(40));
        }
        self.conn.sync().map_err(err)
    }
}

pub const KEY_CONTROL_L: u32 = 0xffe3;
pub const KEY_Q: u32 = 0x0071;
