//! The source files of a compilation and the offsets that address them.
//!
//! Every file gets its own range of `u32` byte offsets in one [`SourceMap`], so a [`Span`]
//! stays 8 bytes and still says which file it is in: the one whose range holds the span's
//! start (`spec/modules.md` §8). Files are one offset apart, so an empty span at the very end
//! of a file still belongs to it.

use crate::span::Span;

/// One source file and where its offsets start.
#[derive(Debug)]
pub struct SourceFile {
    /// The file's path, as diagnostics show it.
    pub name: String,
    /// The file's text.
    pub text: String,
    /// The offset of the file's first byte.
    pub start: u32,
}

impl SourceFile {
    /// The offset one past the file's last byte.
    pub fn end(&self) -> u32 {
        let len = u32::try_from(self.text.len()).unwrap_or(u32::MAX);
        self.start.saturating_add(len)
    }

    /// Whether `offset` is in this file, its end included.
    pub fn contains(&self, offset: u32) -> bool {
        self.start <= offset && offset <= self.end()
    }

    /// The text `span` covers, which must be in this file.
    pub fn text_of(&self, span: Span) -> &str {
        &self.text[(span.start - self.start) as usize..(span.end - self.start) as usize]
    }
}

/// Names a file in a [`SourceMap`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FileId(u32);

impl FileId {
    /// The file's position in the map, counting from 0 in the order files were added.
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// Every source file of a compilation, each with its own range of offsets.
#[derive(Debug, Default)]
pub struct SourceMap {
    files: Vec<SourceFile>,
}

/// The files of a compilation together are larger than 4 GiB, so their offsets do not fit
/// in a `u32`.
#[derive(Debug, PartialEq, Eq)]
pub struct TooLarge;

impl SourceMap {
    /// A map holding only `text`, named `name`, starting at offset 0.
    pub fn single(name: impl Into<String>, text: impl Into<String>) -> Self {
        let mut map = SourceMap::default();
        map.add(name, text).expect("the first file always fits");
        map
    }

    /// Adds a file after the others and returns its id.
    pub fn add(
        &mut self,
        name: impl Into<String>,
        text: impl Into<String>,
    ) -> Result<FileId, TooLarge> {
        let text = text.into();
        let start = match self.files.last() {
            None => 0,
            Some(last) => last.end().checked_add(1).ok_or(TooLarge)?,
        };
        // The first file may be too large on its own, which the lexer reports. A later one
        // must fit after the others.
        let fits = u32::try_from(text.len())
            .ok()
            .and_then(|len| start.checked_add(len))
            .is_some();
        if start > 0 && !fits {
            return Err(TooLarge);
        }
        let id = FileId(self.files.len() as u32);
        self.files.push(SourceFile {
            name: name.into(),
            text,
            start,
        });
        Ok(id)
    }

    /// The file with this id.
    pub fn file(&self, id: FileId) -> &SourceFile {
        &self.files[id.0 as usize]
    }

    /// Every file, in the order they were added.
    pub fn files(&self) -> &[SourceFile] {
        &self.files
    }

    /// The id of the file containing `offset`. Panics if the map is empty.
    pub fn find(&self, offset: u32) -> FileId {
        assert!(!self.files.is_empty(), "no source files");
        let index = self.files.partition_point(|f| f.start <= offset);
        FileId(index.saturating_sub(1) as u32)
    }

    /// The file containing `offset`. Panics if the map is empty.
    pub fn lookup(&self, offset: u32) -> &SourceFile {
        self.file(self.find(offset))
    }

    /// The text `span` covers.
    pub fn text_of(&self, span: Span) -> &str {
        self.lookup(span.start).text_of(span)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_get_their_own_offsets() {
        let mut map = SourceMap::default();
        let a = map.add("a.nv", "abc").unwrap();
        let b = map.add("b.nv", "de").unwrap();
        assert_eq!(map.file(a).start, 0);
        assert_eq!(map.file(b).start, 4);
        assert_eq!(map.lookup(0).name, "a.nv");
        // The end of a file is still in it.
        assert_eq!(map.lookup(3).name, "a.nv");
        assert_eq!(map.lookup(4).name, "b.nv");
        assert_eq!(map.lookup(6).name, "b.nv");
        assert_eq!(map.text_of(Span::new(4, 6)), "de");
        assert!(map.file(b).contains(6) && !map.file(b).contains(3));
    }

    #[test]
    fn offsets_must_fit() {
        let mut map = SourceMap::default();
        map.add("a.nv", "x").unwrap();
        map.files[0].start = u32::MAX - 1;
        assert_eq!(map.add("b.nv", "y"), Err(TooLarge));
    }
}
