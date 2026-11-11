use crate::VamCoreError;

/// Physical states of a single MicroVM session, matching core/lifecycle.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmState {
    Uninitialized,
    Configured,
    Booting,
    Running,
    Paused,
    Halted,
    Reaped,
}

impl VmState {
    /// Validates if a transition from `self` to `target` is valid.
    pub fn can_transition_to(self, target: VmState) -> bool {
        match (self, target) {
            (VmState::Uninitialized, VmState::Configured) => true,
            (VmState::Configured, VmState::Booting) => true,
            (VmState::Booting, VmState::Running) => true,
            (VmState::Running, VmState::Paused) => true,
            (VmState::Paused, VmState::Running) => true, // Resume

            // Graceful or ungraceful exit
            (VmState::Booting, VmState::Halted)
            | (VmState::Running, VmState::Halted)
            | (VmState::Paused, VmState::Halted) => true,

            (VmState::Halted, VmState::Reaped) => true,

            // Force-cleanup / GC is allowed from any state to Reaped
            (_, VmState::Reaped) => true,
            _ => false,
        }
    }
}

/// A wrapper that manages type-safe transitions and prevents invalid flows.
pub struct VmStateMachine {
    current: VmState,
}

impl Default for VmStateMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl VmStateMachine {
    pub fn new() -> Self {
        Self {
            current: VmState::Uninitialized,
        }
    }

    pub fn current(&self) -> VmState {
        self.current
    }

    pub fn transition_to(&mut self, target: VmState) -> Result<(), VamCoreError> {
        if self.current.can_transition_to(target) {
            self.current = target;
            Ok(())
        } else {
            Err(VamCoreError::InvalidConfig(format!(
                "Illegal state transition from {:?} to {:?}",
                self.current, target
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_state_transitions() {
        let mut fsm = VmStateMachine::new();
        assert_eq!(fsm.current(), VmState::Uninitialized);

        fsm.transition_to(VmState::Configured).unwrap();
        fsm.transition_to(VmState::Booting).unwrap();
        fsm.transition_to(VmState::Running).unwrap();
        fsm.transition_to(VmState::Paused).unwrap();
        fsm.transition_to(VmState::Running).unwrap(); // Resume
        fsm.transition_to(VmState::Halted).unwrap();
        fsm.transition_to(VmState::Reaped).unwrap();
    }

    #[test]
    fn test_invalid_transitions() {
        let mut fsm = VmStateMachine::new();
        assert!(fsm.transition_to(VmState::Running).is_err()); // Cannot bypass Configured/Booting
    }

    #[test]
    fn test_force_cleanup_is_always_allowed() {
        let mut fsm = VmStateMachine::new();
        fsm.transition_to(VmState::Configured).unwrap();
        fsm.transition_to(VmState::Reaped).unwrap(); // Force-kill/GC from Configured
        assert_eq!(fsm.current(), VmState::Reaped);
    }
}
