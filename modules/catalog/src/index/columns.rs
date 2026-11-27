//! Плоские массивы слоя Б в файле: порядки сортировок, позиции, `SortMeta`, даты, известность
//! людей. Файл отображается в память (mmap Tantivy, `OwnedBytes`): в куче процесса его нет,
//! в RAM — только прочитанные страницы, и ОС вытесняет их при нехватке памяти. Своего `unsafe`
//! нет: байты в `&[T]` превращает `bytemuck` (с проверкой выравнивания и длины).
//!
//! Формат: `MAGIC` (8 байт), число колонок `n` (u64), `n` пар (смещение, длина в байтах) — u64,
//! затем данные колонок, каждая с границы 8 байт. Порядок байт — little-endian (x86/ARM).

use std::fmt;
use std::io::Write;
use std::marker::PhantomData;
use std::ops::Deref;
use std::path::Path;

use bytemuck::Pod;
use tantivy::directory::{Directory, MmapDirectory, OwnedBytes};

const MAGIC: &[u8; 8] = b"NXCOLS01";

/// Массив: в куче (только что собран) или в отображённом файле.
pub enum Column<T> {
    Heap(Vec<T>),
    Mapped(OwnedBytes, PhantomData<T>),
}

impl<T: Pod> Deref for Column<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        match self {
            Self::Heap(v) => v,
            // Выравнивание и длина проверены в `Columns::get`.
            Self::Mapped(bytes, _) => bytemuck::cast_slice(bytes.as_slice()),
        }
    }
}

impl<T: Pod> Column<T> {
    /// Байт в куче (у отображённой — 0).
    pub fn heap_bytes(&self) -> u64 {
        match self {
            Self::Heap(v) => (v.capacity() * std::mem::size_of::<T>()) as u64,
            Self::Mapped(..) => 0,
        }
    }

    pub fn is_mapped(&self) -> bool {
        matches!(self, Self::Mapped(..))
    }
}

impl<T: Clone> Clone for Column<T> {
    fn clone(&self) -> Self {
        match self {
            Self::Heap(v) => Self::Heap(v.clone()),
            Self::Mapped(b, _) => Self::Mapped(b.clone(), PhantomData),
        }
    }
}

impl<T> Default for Column<T> {
    fn default() -> Self {
        Self::Heap(Vec::new())
    }
}

impl<T> From<Vec<T>> for Column<T> {
    fn from(v: Vec<T>) -> Self {
        Self::Heap(v)
    }
}

impl<T: Pod> fmt::Debug for Column<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = if self.is_mapped() { "mapped" } else { "heap" };
        write!(f, "Column({kind}, {})", self.len())
    }
}

/// Записать колонки файлом атомарно (временный файл и переименование: открытые отображения
/// старого файла остаются целыми).
pub fn write(path: &Path, columns: &[&[u8]]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut out = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
        let header = 8 + 8 + columns.len() * 16;
        let mut offset = align(header);
        out.write_all(MAGIC)?;
        out.write_all(&(columns.len() as u64).to_le_bytes())?;
        for column in columns {
            out.write_all(&(offset as u64).to_le_bytes())?;
            out.write_all(&(column.len() as u64).to_le_bytes())?;
            offset = align(offset + column.len());
        }
        let mut at = header;
        for column in columns {
            pad(&mut out, &mut at)?;
            out.write_all(column)?;
            at += column.len();
        }
        out.flush()?;
    }
    std::fs::rename(&tmp, path)
}

fn align(n: usize) -> usize {
    n.div_ceil(8) * 8
}

fn pad(out: &mut impl Write, at: &mut usize) -> std::io::Result<()> {
    let next = align(*at);
    out.write_all(&[0u8; 8][..next - *at])?;
    *at = next;
    Ok(())
}

/// Открытый файл колонок.
pub struct Columns {
    bytes: OwnedBytes,
    ranges: Vec<(usize, usize)>,
}

impl Columns {
    /// Отобразить файл `name` из каталога `dir`.
    pub fn open(dir: &Path, name: &str) -> std::io::Result<Self> {
        let invalid = |m: String| std::io::Error::new(std::io::ErrorKind::InvalidData, m);
        let directory = MmapDirectory::open(dir).map_err(|e| invalid(e.to_string()))?;
        let bytes = directory
            .open_read(Path::new(name))
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::NotFound, e.to_string()))?
            .read_bytes()?;
        let data = bytes.as_slice();
        if data.len() < 16 || &data[..8] != MAGIC {
            return Err(invalid(format!("{name}: unknown format")));
        }
        let word = |at: usize| -> std::io::Result<usize> {
            let b: [u8; 8] = data
                .get(at..at + 8)
                .and_then(|s| s.try_into().ok())
                .ok_or_else(|| invalid(format!("{name}: truncated header")))?;
            Ok(u64::from_le_bytes(b) as usize)
        };
        let n = word(8)?;
        let mut ranges = Vec::with_capacity(n);
        for i in 0..n {
            let (start, len) = (word(16 + i * 16)?, word(24 + i * 16)?);
            if start % 8 != 0 || start + len > data.len() {
                return Err(invalid(format!("{name}: column {i} is out of bounds")));
            }
            ranges.push((start, len));
        }
        Ok(Self { bytes, ranges })
    }

    pub fn len(&self) -> usize {
        self.ranges.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    /// Колонка `i` как массив `T` (без копирования).
    pub fn get<T: Pod>(&self, i: usize) -> std::io::Result<Column<T>> {
        let invalid = |m: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, m.to_string());
        let &(start, len) = self
            .ranges
            .get(i)
            .ok_or_else(|| invalid("no such column"))?;
        let bytes = self.bytes.slice(start..start + len);
        // Проверка один раз: дальше `Deref` приводит без ошибок.
        bytemuck::try_cast_slice::<u8, T>(bytes.as_slice())
            .map_err(|_| invalid("column is misaligned or has a wrong length"))?;
        Ok(Column::Mapped(bytes, PhantomData))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_round_trip_without_copy() {
        let dir = std::env::temp_dir().join(format!("nexus-cols-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let a: Vec<u32> = (0..1001).collect();
        let b: Vec<u64> = vec![7, 8, 9];
        let c: Vec<u8> = vec![1, 2, 3];
        write(
            &dir.join("x.cols"),
            &[
                bytemuck::cast_slice(&a),
                bytemuck::cast_slice(&c),
                bytemuck::cast_slice(&b),
            ],
        )
        .unwrap();
        let cols = Columns::open(&dir, "x.cols").unwrap();
        assert_eq!(cols.len(), 3);
        let a2: Column<u32> = cols.get(0).unwrap();
        let b2: Column<u64> = cols.get(2).unwrap();
        assert!(a2.is_mapped());
        assert_eq!(&a2[..], &a[..]);
        assert_eq!(&b2[..], &b[..]);
        assert_eq!(a2.heap_bytes(), 0);
        // Длина не делится на размер элемента — ошибка, а не паника.
        assert!(cols.get::<u64>(1).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
