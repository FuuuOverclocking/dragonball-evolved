use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use crate::{MAX_FDS, invalid};

const INLINE: usize = 512;

/// Ordered, owned descriptors: either all unnamed or all named by a matching X-FD header.
#[derive(Debug, Default)]
pub struct Fds {
    entries: Vec<Entry>,
}

#[derive(Debug)]
struct Entry {
    fd: Option<OwnedFd>,
    name: Option<Box<str>>,
}

impl Fds {
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn push(&mut self, fd: OwnedFd) -> io::Result<usize> {
        self.insert(None, fd)
    }

    pub fn push_named(&mut self, name: &str, fd: OwnedFd) -> io::Result<usize> {
        if !valid_name(name) || self.entries.iter().any(|e| e.name.as_deref() == Some(name)) {
            return Err(invalid("invalid or duplicate FD name"));
        }
        self.insert(Some(name.into()), fd)
    }

    fn insert(&mut self, name: Option<Box<str>>, fd: OwnedFd) -> io::Result<usize> {
        if self.len() == MAX_FDS {
            return Err(invalid("too many file descriptors"));
        }
        if self
            .entries
            .first()
            .is_some_and(|e| e.name.is_some() != name.is_some())
        {
            return Err(invalid("named and unnamed descriptors cannot be mixed"));
        }
        let index = self.len();
        self.entries.push(Entry { fd: Some(fd), name });
        Ok(index)
    }

    pub fn get(&self, index: usize) -> Option<BorrowedFd<'_>> {
        self.entries.get(index)?.fd.as_ref().map(AsFd::as_fd)
    }

    pub fn get_named(&self, name: &str) -> Option<BorrowedFd<'_>> {
        self.get(self.index_of(name)?)
    }

    /// Taking a descriptor leaves its index vacant; a body with vacant slots cannot be sent.
    pub fn take(&mut self, index: usize) -> Option<OwnedFd> {
        self.entries.get_mut(index)?.fd.take()
    }

    pub fn take_named(&mut self, name: &str) -> Option<OwnedFd> {
        self.take(self.index_of(name)?)
    }

    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.entries
            .iter()
            .position(|entry| entry.name.as_deref() == Some(name))
    }

    pub fn name(&self, index: usize) -> Option<&str> {
        self.entries.get(index)?.name.as_deref()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub(crate) fn bind_names(&mut self, names: &str) -> io::Result<()> {
        let mut count = 0;
        for (index, name) in names.split(',').map(str::trim).enumerate() {
            if index >= self.len() || !valid_name(name) {
                return Err(invalid("X-FD names must match received descriptors"));
            }
            if self.entries[..index]
                .iter()
                .any(|e| e.name.as_deref() == Some(name))
            {
                return Err(invalid("duplicate X-FD name"));
            }
            let entry = &mut self.entries[index];
            if let Some(existing) = &entry.name {
                if existing.as_ref() != name {
                    return Err(invalid("X-FD disagrees with descriptor names"));
                }
            } else {
                entry.name = Some(name.into());
            }
            count += 1;
        }
        if count != self.len() {
            return Err(invalid("X-FD descriptor count mismatch"));
        }
        Ok(())
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

#[derive(Debug)]
pub struct Body {
    inline: [u8; INLINE],
    heap: Vec<u8>,
    len: usize,
    fds: Fds,
}

impl Default for Body {
    fn default() -> Self {
        Self::empty()
    }
}

impl Body {
    pub fn empty() -> Self {
        Self {
            inline: [0; INLINE],
            heap: Vec::new(),
            len: 0,
            fds: Fds::default(),
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn as_bytes(&self) -> &[u8] {
        if self.len <= INLINE {
            &self.inline[..self.len]
        } else {
            &self.heap
        }
    }

    pub fn fds(&self) -> &Fds {
        &self.fds
    }

    pub fn fds_mut(&mut self) -> &mut Fds {
        &mut self.fds
    }

    pub fn into_bytes(self) -> Vec<u8> {
        if self.len <= INLINE {
            self.inline[..self.len].to_vec()
        } else {
            self.heap
        }
    }

    #[cfg(feature = "json")]
    pub fn json<T: serde::Serialize + ?Sized>(value: &T) -> serde_json::Result<Self> {
        let mut body = Self::empty();
        serde_json::to_writer(&mut body, value)?;
        Ok(body)
    }

    #[cfg(feature = "json")]
    pub fn from_json<'a, T: serde::Deserialize<'a>>(&'a self) -> serde_json::Result<T> {
        serde_json::from_slice(self.as_bytes())
    }

    pub(crate) fn receive(len: usize, fds: Fds) -> Self {
        Self {
            inline: [0; INLINE],
            heap: if len > INLINE {
                vec![0; len]
            } else {
                Vec::new()
            },
            len,
            fds,
        }
    }

    pub(crate) fn resize(&mut self, len: usize) {
        self.heap.resize(if len > INLINE { len } else { 0 }, 0);
        self.len = len;
    }

    pub(crate) fn bytes_mut(&mut self) -> &mut [u8] {
        if self.len <= INLINE {
            &mut self.inline[..self.len]
        } else {
            &mut self.heap
        }
    }
}

impl AsRef<[u8]> for Body {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl From<&[u8]> for Body {
    fn from(bytes: &[u8]) -> Self {
        let mut body = Self::receive(bytes.len(), Fds::default());
        body.bytes_mut().copy_from_slice(bytes);
        body
    }
}

impl<const N: usize> From<&[u8; N]> for Body {
    fn from(bytes: &[u8; N]) -> Self {
        Self::from(bytes.as_slice())
    }
}

impl From<&str> for Body {
    fn from(value: &str) -> Self {
        Self::from(value.as_bytes())
    }
}

impl From<String> for Body {
    fn from(value: String) -> Self {
        Self::from(value.into_bytes())
    }
}

impl From<Vec<u8>> for Body {
    fn from(bytes: Vec<u8>) -> Self {
        if bytes.len() <= INLINE {
            Self::from(bytes.as_slice())
        } else {
            Self {
                len: bytes.len(),
                heap: bytes,
                ..Self::empty()
            }
        }
    }
}

impl io::Write for Body {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let len = self
            .len
            .checked_add(bytes.len())
            .ok_or_else(|| invalid("body length overflow"))?;
        if len <= INLINE {
            self.inline[self.len..len].copy_from_slice(bytes);
        } else {
            if self.len <= INLINE {
                self.heap.reserve(len);
                self.heap.extend_from_slice(&self.inline[..self.len]);
            }
            self.heap.extend_from_slice(bytes);
        }
        self.len = len;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
