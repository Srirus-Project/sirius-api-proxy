//! Process-local ownership must end even if a fork/duplicate retains the open description.
pub(crate) struct Exclusive(std::fs::File);
impl Exclusive {
    pub(crate) fn acquire(file: std::fs::File) -> Result<Self, std::fs::TryLockError> {
        file.try_lock()?;
        Ok(Self(file))
    }
    #[cfg(test)]
    pub(crate) fn duplicate(&self) -> std::io::Result<std::fs::File> {
        self.0.try_clone()
    }
}
impl Drop for Exclusive {
    fn drop(&mut self) {
        // Explicit unlock prevents a concurrent fork's inherited description from
        // extending ownership until exec, including early returns after acquisition.
        let _ = self.0.unlock();
    }
}
