//! In-memory anchor transitions at the publisher's checked mutation boundaries.

use super::*;

impl AnchoredRelease {
    /// Only a checked selection transaction can advance this one parent baseline.
    pub(crate) fn selection_parent_transition(
        &mut self,
        transition: &crate::selection_intent::macos::preparation::ParentChange,
    ) -> Result<(), &'static str> {
        crate::current_selection::parse_target(self.components.last().ok_or(UNSAFE)?.to_bytes())?;
        let (parent, _) = self.publication_walk(None)?;
        let index = self.directories.len() - 2;
        transition.check(&self.directories[index].fingerprint, &parent)?;
        self.directories[index].fingerprint = parent;
        self.revalidate()
    }

    pub(crate) fn publication_parent(&self) -> &File {
        &self.directories[self.directories.len() - 2].file
    }

    pub(crate) fn publication_directory(&self) -> &File {
        &self.directories[self.directories.len() - 1].file
    }

    /// Refresh only parent metadata after preflight or a checked receipt-sibling change.
    /// The original stage fingerprint and every original artifact remain fixed.
    pub(crate) fn publication_refresh_parent(&mut self) -> Result<(), &'static str> {
        self.require_stage_name()?;
        let (parent, _) = self.publication_walk(None)?;
        let index = self.directories.len() - 2;
        self.directories[index].fingerprint = parent;
        self.revalidate()
    }

    /// Rebind only the original stage inode at its digest-derived sibling after rename.
    pub(crate) fn publication_rebind(&mut self, digest: &[u8; 32]) -> Result<(), &'static str> {
        self.require_stage_name()?;
        let target = crate::current_selection::encode_target(digest);
        let name = CString::new(target.as_slice()).map_err(|_| UNSAFE)?;
        let (parent, stage) = self.publication_walk(Some(&name))?;
        let length = self.directories.len();
        self.directories[length - 2].fingerprint = parent;
        self.directories[length - 1].fingerprint = stage;
        self.path
            .set_file_name(std::ffi::OsStr::from_bytes(&target));
        *self.components.last_mut().ok_or(UNSAFE)? = name;
        self.revalidate()
    }

    fn require_stage_name(&self) -> Result<(), &'static str> {
        let name = self.components.last().ok_or(UNSAFE)?.to_bytes();
        if name.len() != 39
            || !name.starts_with(b".stage-")
            || !name[7..]
                .iter()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
        {
            return Err(UNSAFE);
        }
        Ok(())
    }

    fn publication_walk(
        &self,
        renamed: Option<&CStr>,
    ) -> Result<(Fingerprint, Fingerprint), &'static str> {
        let parent_index = self.directories.len() - 2;
        let stage_index = parent_index + 1;
        let mut parent = None;
        let mut stage = None;
        let mut walked = open_at(libc::AT_FDCWD, c"/", true)?;
        for (index, held) in self.directories.iter().enumerate() {
            if index != 0 {
                let name = if index == stage_index {
                    renamed.unwrap_or(&self.components[index - 1])
                } else {
                    &self.components[index - 1]
                };
                walked = open_at(walked.as_raw_fd(), name, true)?;
            }
            let original = directory_metadata(&held.file, self.uid, held.private)?;
            let current = directory_metadata(&walked, self.uid, held.private)?;
            for observed in [&original, &current] {
                let matches = if index == parent_index {
                    directory_matches(observed, &held.fingerprint, false)
                } else if index == stage_index && renamed.is_some() {
                    observed.matches_after_rename(&held.fingerprint)
                } else {
                    directory_matches(observed, &held.fingerprint, held.private)
                };
                if !matches {
                    return Err(UNSAFE);
                }
            }
            if index == parent_index || index == stage_index {
                if original != current {
                    return Err(UNSAFE);
                }
                if index == parent_index {
                    parent = Some(current);
                } else {
                    stage = Some(current);
                }
            }
        }
        self.revalidate_artifacts(&walked)?;
        Ok((parent.ok_or(UNSAFE)?, stage.ok_or(UNSAFE)?))
    }
}
