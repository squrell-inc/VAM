use crate::VamCoreError;
use std::collections::HashSet;
use std::sync::Mutex;

/// Thread-safe Context ID (CID) allocator for guest vsock devices.
/// CID 0, 1, 2 are reserved. CIDs must be between 3 and 0xFFFFFFFF.
pub struct CidAllocator {
    allocated: Mutex<HashSet<u32>>,
    min_cid: u32,
    max_cid: u32,
}

impl Default for CidAllocator {
    /// Creates a new CID allocator with the default range (3 to 65535).
    fn default() -> Self {
        Self::new(3, 65535)
    }
}

impl CidAllocator {
    /// Creates a new CID allocator with a specific CID range.
    pub fn new(min_cid: u32, max_cid: u32) -> Self {
        assert!(min_cid >= 3, " CIDs below 3 are reserved");
        Self {
            allocated: Mutex::new(HashSet::new()),
            min_cid,
            max_cid,
        }
    }

    /// Allocates an available CID. Returns CidExhausted error if none are available.
    pub fn allocate(&self) -> Result<u32, VamCoreError> {
        let mut allocated = self
            .allocated
            .lock()
            .map_err(|e| VamCoreError::CleanupFailed(format!("CID lock poisoned: {e}")))?;

        for cid in self.min_cid..=self.max_cid {
            if !allocated.contains(&cid) {
                allocated.insert(cid);
                return Ok(cid);
            }
        }

        Err(VamCoreError::CidExhausted)
    }

    /// Reserves a specific CID, failing if it's already allocated or out of range.
    pub fn reserve(&self, cid: u32) -> Result<(), VamCoreError> {
        let mut allocated = self
            .allocated
            .lock()
            .map_err(|e| VamCoreError::CleanupFailed(format!("CID lock poisoned: {e}")))?;
        if cid < self.min_cid || cid > self.max_cid {
            return Err(VamCoreError::InvalidConfig(format!(
                "CID {} is out of range",
                cid
            )));
        }
        if allocated.contains(&cid) {
            return Err(VamCoreError::InvalidConfig(format!(
                "CID {} is already allocated",
                cid
            )));
        }
        allocated.insert(cid);
        Ok(())
    }

    /// Releases a previously allocated CID, making it available for reuse.
    pub fn release(&self, cid: u32) {
        if let Ok(mut allocated) = self.allocated.lock() {
            allocated.remove(&cid);
        }
    }

    /// Returns true if a specific CID is currently allocated.
    pub fn is_allocated(&self, cid: u32) -> bool {
        if let Ok(allocated) = self.allocated.lock() {
            allocated.contains(&cid)
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cid_allocation_lifecycle() {
        let allocator = CidAllocator::new(3, 5);

        let c1 = allocator.allocate().unwrap();
        let c2 = allocator.allocate().unwrap();
        let c3 = allocator.allocate().unwrap();

        assert_eq!(c1, 3);
        assert_eq!(c2, 4);
        assert_eq!(c3, 5);
        assert!(allocator.is_allocated(3));

        assert!(allocator.allocate().is_err()); // exhausted

        allocator.release(4);
        assert!(!allocator.is_allocated(4));

        let c4 = allocator.allocate().unwrap();
        assert_eq!(c4, 4); // reused
    }

    #[test]
    fn test_cid_reservation() {
        let allocator = CidAllocator::new(3, 5);
        allocator.reserve(4).unwrap();
        assert!(allocator.is_allocated(4));
        assert!(allocator.reserve(4).is_err());
        assert!(allocator.reserve(2).is_err()); // out of range

        let c1 = allocator.allocate().unwrap();
        assert_eq!(c1, 3); // 4 was reserved, so it skipped to next/previous free!
    }
}
