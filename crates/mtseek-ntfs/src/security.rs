//! Security descriptors, callers' tokens and access checks (ADR 0014).
//!
//! The content index records each file's and folder's security descriptor (owner, group,
//! DACL and mandatory label, as self-relative bytes). A search then decides what a caller
//! may see with [`Token::allows`]: `AccessCheck` of the recorded descriptor against the
//! caller's token, with the access `rg` itself would ask for ([`READ`] for files, [`LIST`]
//! for folders). Nothing is opened as the caller; an identification-level token is enough.
//!
//! Descriptors come from index files, so they are checked for structure in safe code
//! ([`well_formed`]) before Windows reads them.

use std::fs::File;
use std::io;
use std::os::windows::io::{AsHandle, OwnedHandle};
use std::path::Path;

use crate::sys;

/// The access `rg` asks for when it opens a file: `FILE_GENERIC_READ`.
pub const READ: u32 = 0x0012_0089;
/// The access listing a folder needs: `FILE_LIST_DIRECTORY | SYNCHRONIZE`.
pub const LIST: u32 = 0x0010_0001;

/// The descriptor of an open file or folder (the handle needs `READ_CONTROL`, which
/// `File::open` asks for).
pub fn descriptor_of(file: &impl AsHandle) -> io::Result<Vec<u8>> {
    sys::handle_descriptor(file.as_handle())
}

/// The descriptor of the file or folder at `path`, read the same way as [`descriptor_of`]
/// (through a handle opened for `READ_CONTROL` only), so the two give identical bytes.
pub fn descriptor_at(path: &Path) -> io::Result<Vec<u8>> {
    const READ_CONTROL: u32 = 0x2_0000;
    const BACKUP_SEMANTICS: u32 = 0x0200_0000; // needed to open folders
    let handle = sys::open(path.as_os_str(), READ_CONTROL, BACKUP_SEMANTICS)?;
    sys::handle_descriptor(handle.as_handle())
}

/// Replaces the DACL of `path` with the one in `sddl` (`D:P(...)`), protected from
/// inheritance. Tests use it to take access away from themselves.
pub fn set_dacl(path: &Path, sddl: &str) -> io::Result<()> {
    let descriptor = sys::security_descriptor(sddl)?;
    sys::set_path_security(path.as_os_str(), &descriptor, false)
}

/// The descriptor that SDDL `sddl` describes, as self-relative bytes.
pub fn descriptor_from_sddl(sddl: &str) -> io::Result<Vec<u8>> {
    Ok(sys::security_descriptor(sddl)?.to_bytes())
}

/// A caller's token: who it is and which groups it holds. It can be used for access checks
/// and nothing else.
#[derive(Debug)]
pub struct Token(OwnedHandle);

impl Token {
    /// This process's own identity.
    pub fn current() -> io::Result<Self> {
        sys::own_identification_token().map(Self)
    }

    /// The identity of the client connected to server pipe `pipe`. Call it after reading
    /// the client's first message: Windows only identifies a client that has written.
    pub fn of_client(pipe: &File) -> io::Result<Self> {
        sys::pipe_client_token(pipe.as_handle()).map(Self)
    }

    /// Whether `descriptor` grants this caller `desired` ([`READ`] or [`LIST`]). A malformed
    /// descriptor, or a check that fails, grants nothing.
    pub fn allows(&self, descriptor: &[u8], desired: u32) -> bool {
        well_formed(descriptor)
            && sys::access_check(self.0.as_handle(), descriptor, desired).unwrap_or(false)
    }

    /// Whether the caller is an elevated administrator.
    pub fn is_admin(&self) -> bool {
        sys::token_is_admin(self.0.as_handle()).unwrap_or(false)
    }

    /// The caller's user SID (`S-1-5-21-...`).
    pub fn user_sid(&self) -> io::Result<String> {
        sys::token_user_sid(self.0.as_handle())
    }
}

/// Whether `d` is a self-relative security descriptor whose every offset and length lies
/// inside it: header, owner and group SIDs, both ACLs and every ACE with its SID. Only such
/// a descriptor is handed to `AccessCheck`.
pub fn well_formed(d: &[u8]) -> bool {
    const SELF_RELATIVE: u16 = 0x8000;
    const SACL_PRESENT: u16 = 0x0010;
    const DACL_PRESENT: u16 = 0x0004;
    if d.len() < 20 || d[0] != 1 {
        return false;
    }
    let control = u16::from_le_bytes([d[2], d[3]]);
    if control & SELF_RELATIVE == 0 {
        return false;
    }
    let at = |i: usize| u32::from_le_bytes([d[i], d[i + 1], d[i + 2], d[i + 3]]) as usize;
    let (owner, group, sacl, dacl) = (at(4), at(8), at(12), at(16));
    let sid_ok = |off: usize, end: usize| sid_len(d, off).is_some_and(|n| off + n <= end);
    (owner == 0 || sid_ok(owner, d.len()))
        && (group == 0 || sid_ok(group, d.len()))
        && (control & SACL_PRESENT == 0 || sacl == 0 || acl_ok(d, sacl))
        && (control & DACL_PRESENT == 0 || dacl == 0 || acl_ok(d, dacl))
}

/// The length of the SID at `off`, if its header is inside `d`.
fn sid_len(d: &[u8], off: usize) -> Option<usize> {
    let header = d.get(off..off.checked_add(8)?)?;
    let count = usize::from(header[1]);
    (header[0] == 1 && count <= 15).then_some(8 + 4 * count)
}

/// Whether the ACL at `off` and each of its ACEs lie inside `d`.
fn acl_ok(d: &[u8], off: usize) -> bool {
    let Some(header) = off.checked_add(8).and_then(|end| d.get(off..end)) else {
        return false;
    };
    let size = usize::from(u16::from_le_bytes([header[2], header[3]]));
    let count = usize::from(u16::from_le_bytes([header[4], header[5]]));
    let end = off + size;
    if size < 8 || end > d.len() {
        return false;
    }
    let mut ace = off + 8;
    for _ in 0..count {
        let Some(h) = d.get(ace..ace + 4).filter(|_| ace + 4 <= end) else {
            return false;
        };
        let kind = h[0];
        let ace_size = usize::from(u16::from_le_bytes([h[2], h[3]]));
        if ace_size < 8 || ace + ace_size > end {
            return false;
        }
        // The SID's offset in the ACE: after the mask for plain ACEs, after the flags and
        // the GUIDs they announce for object ACEs.
        let sid_at = match kind {
            0x05..=0x08 | 0x0B | 0x0C | 0x0F | 0x10 => {
                if ace_size < 12 {
                    return false;
                }
                let flags = u32::from_le_bytes([d[ace + 8], d[ace + 9], d[ace + 10], d[ace + 11]]);
                12 + if flags & 1 != 0 { 16 } else { 0 } + if flags & 2 != 0 { 16 } else { 0 }
            }
            _ => 8,
        };
        let sid = ace + sid_at;
        if sid_len(d, sid).is_none_or(|n| sid + n > ace + ace_size) {
            return false;
        }
        ace += ace_size;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn me() -> String {
        crate::pipe::user_sid().unwrap()
    }

    #[test]
    fn descriptors_of_real_files_are_well_formed_and_grant_their_owner() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "x").unwrap();
        let by_handle = descriptor_of(&File::open(&path).unwrap()).unwrap();
        let by_path = descriptor_at(&path).unwrap();
        assert_eq!(by_handle, by_path);
        assert!(well_formed(&by_path));
        let token = Token::current().unwrap();
        assert!(token.allows(&by_path, READ));
        assert!(token.allows(&descriptor_at(dir.path()).unwrap(), LIST));
        assert_eq!(token.user_sid().unwrap(), me());
    }

    #[test]
    fn deny_entries_decide() {
        let token = Token::current().unwrap();
        let sid = me();
        let allow = descriptor_from_sddl(&format!("O:{sid}G:{sid}D:P(A;;FA;;;{sid})")).unwrap();
        let deny_read =
            descriptor_from_sddl(&format!("O:{sid}G:{sid}D:P(D;;0x1;;;{sid})(A;;FA;;;{sid})"))
                .unwrap();
        let nobody = descriptor_from_sddl(&format!("O:{sid}G:{sid}D:P")).unwrap();
        assert!(token.allows(&allow, READ) && token.allows(&allow, LIST));
        assert!(!token.allows(&deny_read, READ) && !token.allows(&deny_read, LIST));
        assert!(!token.allows(&nobody, READ));
        assert!(!token.allows(&[], READ), "no descriptor grants nothing");
    }

    #[test]
    fn taking_read_away_from_yourself_shows_in_the_descriptor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.txt");
        std::fs::write(&path, "x").unwrap();
        let sid = me();
        set_dacl(&path, &format!("D:P(D;;0x1;;;{sid})(A;;FA;;;{sid})")).unwrap();
        assert!(std::fs::read(&path).is_err());
        let token = Token::current().unwrap();
        assert!(!token.allows(&descriptor_at(&path).unwrap(), READ));
        set_dacl(&path, &format!("D:P(A;;FA;;;{sid})")).unwrap();
        assert!(token.allows(&descriptor_at(&path).unwrap(), READ));
    }

    proptest! {
        /// Cutting or changing a descriptor never makes a check read outside it: it is
        /// refused, or it is still well formed and checked.
        #[test]
        fn damaged_descriptors_are_refused_or_checked(at in any::<prop::sample::Index>(), value in any::<u8>(), cut in any::<prop::sample::Index>()) {
            let sid = me();
            let good = descriptor_from_sddl(&format!(
                "O:{sid}G:{sid}D:P(D;;0x1;;;S-1-5-7)(A;OICI;FA;;;{sid})(A;;0x1200a9;;;BU)S:(ML;;NW;;;LW)"
            )).unwrap();
            prop_assert!(well_formed(&good));
            let mut bad = good.clone();
            let i = at.index(bad.len());
            bad[i] = value;
            bad.truncate(cut.index(bad.len() + 1));
            let token = Token::current().unwrap();
            let _ = token.allows(&bad, READ);
        }
    }
}
