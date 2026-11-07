//! Строгий разбор формата safetensors без исполнения кода.
//!
//! Формат — 8 байт длины заголовка (u64 LE), JSON-заголовок с таблицей
//! тензоров и сырые данные. Поддерживается только `F32` (оба закреплённых
//! чекпойнта хранятся в нём). Смещения обязаны покрывать область данных
//! без пересечений и дыр; любое отклонение — отказ, а не попытка
//! «дочитать». Pickle и произвольный код при загрузке не участвуют.

use std::collections::BTreeMap;

use serde::Deserialize;

/// Предел заголовка: защищает от подставленного файла с гигантским JSON.
const MAX_HEADER_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatError(pub String);

impl std::fmt::Display for FormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "safetensors: {}", self.0)
    }
}

impl std::error::Error for FormatError {}

fn bad(message: impl Into<String>) -> FormatError {
    FormatError(message.into())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    dtype: String,
    shape: Vec<usize>,
    data_offsets: [usize; 2],
}

/// Тензор f32: форма и значения в порядке строк.
#[derive(Debug, Clone, PartialEq)]
pub struct Tensor {
    pub shape: Vec<usize>,
    pub data: Vec<f32>,
}

/// Разбирает весь файл. Значения копируются в `Vec<f32>`, исходные байты
/// можно освободить.
pub fn parse(bytes: &[u8]) -> Result<BTreeMap<String, Tensor>, FormatError> {
    let header_len = bytes
        .get(..8)
        .map(|b| u64::from_le_bytes(b.try_into().expect("8 байт")))
        .ok_or_else(|| bad("файл короче заголовка"))?;
    if header_len > MAX_HEADER_BYTES || header_len as usize > bytes.len() - 8 {
        return Err(bad("недопустимая длина заголовка"));
    }
    let header_end = 8 + header_len as usize;
    let header: BTreeMap<String, serde_json::Value> = serde_json::from_slice(&bytes[8..header_end])
        .map_err(|e| bad(format!("заголовок: {e}")))?;
    let data = &bytes[header_end..];
    let mut spans = Vec::new();
    let mut tensors = BTreeMap::new();
    for (name, value) in header {
        if name == "__metadata__" {
            continue;
        }
        let entry: Entry =
            serde_json::from_value(value).map_err(|e| bad(format!("{name}: {e}")))?;
        if entry.dtype != "F32" {
            return Err(bad(format!(
                "{name}: тип {} не поддерживается",
                entry.dtype
            )));
        }
        let [start, end] = entry.data_offsets;
        let count = entry
            .shape
            .iter()
            .try_fold(1usize, |acc, d| acc.checked_mul(*d))
            .ok_or_else(|| bad(format!("{name}: переполнение формы")))?;
        if start > end || end > data.len() || end - start != count * 4 {
            return Err(bad(format!("{name}: смещения не согласованы с формой")));
        }
        spans.push((start, end));
        let values: Vec<f32> = data[start..end]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect();
        if values.iter().any(|v| !v.is_finite()) {
            return Err(bad(format!("{name}: неконечные значения")));
        }
        tensors.insert(
            name,
            Tensor {
                shape: entry.shape,
                data: values,
            },
        );
    }
    spans.sort_unstable();
    let mut cursor = 0;
    for (start, end) in spans {
        if start != cursor {
            return Err(bad("области тензоров пересекаются или оставляют дыры"));
        }
        cursor = end;
    }
    if cursor != data.len() {
        return Err(bad("после тензоров остались лишние байты"));
    }
    Ok(tensors)
}

/// Сериализация тех же тензоров (детерминированный порядок имён). Нужна
/// тестам и синтетическим моделям; рабочие веса только читаются.
pub fn serialize(tensors: &BTreeMap<String, Tensor>) -> Vec<u8> {
    let mut header = serde_json::Map::new();
    let mut offset = 0usize;
    for (name, tensor) in tensors {
        let len = tensor.data.len() * 4;
        header.insert(
            name.clone(),
            serde_json::json!({
                "dtype": "F32",
                "shape": tensor.shape,
                "data_offsets": [offset, offset + len],
            }),
        );
        offset += len;
    }
    let header = serde_json::to_vec(&header).expect("заголовок сериализуем");
    let mut out = Vec::with_capacity(8 + header.len() + offset);
    out.extend_from_slice(&(header.len() as u64).to_le_bytes());
    out.extend_from_slice(&header);
    for tensor in tensors.values() {
        for v in &tensor.data {
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> BTreeMap<String, Tensor> {
        BTreeMap::from([
            (
                "a".to_string(),
                Tensor {
                    shape: vec![2, 2],
                    data: vec![1.0, 2.0, 3.0, 4.0],
                },
            ),
            (
                "b".to_string(),
                Tensor {
                    shape: vec![3],
                    data: vec![-1.0, 0.5, 0.25],
                },
            ),
        ])
    }

    #[test]
    fn round_trip() {
        let bytes = serialize(&sample());
        assert_eq!(parse(&bytes).unwrap(), sample());
    }

    #[test]
    fn rejects_trailing_bytes_holes_and_other_dtypes() {
        let mut bytes = serialize(&sample());
        bytes.push(0);
        assert!(parse(&bytes).is_err());
        let header = br#"{"a":{"dtype":"F16","shape":[1],"data_offsets":[0,2]}}"#;
        let mut f16 = (header.len() as u64).to_le_bytes().to_vec();
        f16.extend_from_slice(header);
        f16.extend_from_slice(&[0, 0]);
        assert!(parse(&f16).unwrap_err().0.contains("F16"));
        let header = br#"{"a":{"dtype":"F32","shape":[1],"data_offsets":[4,8]}}"#;
        let mut hole = (header.len() as u64).to_le_bytes().to_vec();
        hole.extend_from_slice(header);
        hole.extend_from_slice(&[0; 8]);
        assert!(parse(&hole).is_err());
        assert!(parse(&[1, 2, 3]).is_err());
        assert!(parse(&u64::MAX.to_le_bytes()).is_err());
    }

    #[test]
    fn rejects_non_finite_values() {
        let mut tensors = sample();
        tensors.get_mut("b").unwrap().data[1] = f32::NAN;
        assert!(parse(&serialize(&tensors)).is_err());
    }
}
