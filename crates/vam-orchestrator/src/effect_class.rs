//! The effect class of a tool (effect journal): what a repeat of the call does.
//!
//! A class the manifest (or, for MCP, the connection owner) declares wins. Otherwise it is
//! inferred from the side-effect class: read-only is `Read`, anything else is `NonIdempotent`.
//! A vendor hint (`idempotentHint`, `readOnlyHint`) is never an input.

use vam_spec::v1::{EffectClass as ProtoClass, EffectSpec, SideEffectClass};

/// The class the journal acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectClass {
    /// Observational; a repeat is harmless and is not journaled as an effect.
    Read,
    /// A repeat with the same key has the same effect as one call.
    Idempotent,
    /// A repeat does the effect again; an unknown outcome must be checked, never re-sent blindly.
    NonIdempotent,
}

impl EffectClass {
    /// The value stored in the journal row.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Idempotent => "idempotent",
            Self::NonIdempotent => "non_idempotent",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "read" => Some(Self::Read),
            "idempotent" => Some(Self::Idempotent),
            "non_idempotent" => Some(Self::NonIdempotent),
            _ => None,
        }
    }
}

/// Class from the side-effect class alone.
pub fn infer_from_side_effect(side_effect: i32) -> EffectClass {
    if side_effect == SideEffectClass::ReadOnly as i32 {
        EffectClass::Read
    } else {
        EffectClass::NonIdempotent
    }
}

/// The class of a call: the declared one if set, else inferred. A declared `read` on a tool that
/// is not read-only is ignored (the declaration cannot lower a mutating tool to a read); manifest
/// validation rejects that case, this keeps the journal safe if one slips through.
pub fn effective_class(declared: Option<&EffectSpec>, side_effect: i32) -> EffectClass {
    let inferred = infer_from_side_effect(side_effect);
    let Some(spec) = declared else {
        return inferred;
    };
    match ProtoClass::try_from(spec.class).unwrap_or(ProtoClass::Unspecified) {
        ProtoClass::Unspecified => inferred,
        ProtoClass::Read if inferred != EffectClass::Read => inferred,
        ProtoClass::Read => EffectClass::Read,
        ProtoClass::Idempotent => EffectClass::Idempotent,
        ProtoClass::NonIdempotent => EffectClass::NonIdempotent,
    }
}

/// Internal host adapters (`step_complete`, `start_playbook`, `check_playbook_finish`,
/// `run_step`) are journaled as idempotent: the host is its own vendor and re-checks on a re-send.
pub fn internal_adapter_class() -> EffectClass {
    EffectClass::Idempotent
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(class: ProtoClass) -> EffectSpec {
        EffectSpec {
            class: class as i32,
            verify: None,
        }
    }
    const READ: i32 = SideEffectClass::ReadOnly as i32;
    const LOCAL: i32 = SideEffectClass::MutableLocal as i32;
    const EXT: i32 = SideEffectClass::MutableExternal as i32;

    #[test]
    fn inferred_from_side_effect() {
        assert_eq!(effective_class(None, READ), EffectClass::Read);
        assert_eq!(effective_class(None, LOCAL), EffectClass::NonIdempotent);
        assert_eq!(effective_class(None, EXT), EffectClass::NonIdempotent);
        assert_eq!(effective_class(None, 0), EffectClass::NonIdempotent);
    }

    #[test]
    fn unspecified_declaration_falls_back_to_inference() {
        let s = spec(ProtoClass::Unspecified);
        assert_eq!(effective_class(Some(&s), READ), EffectClass::Read);
        assert_eq!(effective_class(Some(&s), EXT), EffectClass::NonIdempotent);
    }

    #[test]
    fn a_declared_class_wins() {
        assert_eq!(
            effective_class(Some(&spec(ProtoClass::Idempotent)), EXT),
            EffectClass::Idempotent
        );
        assert_eq!(
            effective_class(Some(&spec(ProtoClass::NonIdempotent)), READ),
            EffectClass::NonIdempotent
        );
    }

    #[test]
    fn a_declared_read_never_lowers_a_mutating_tool() {
        assert_eq!(
            effective_class(Some(&spec(ProtoClass::Read)), EXT),
            EffectClass::NonIdempotent
        );
        assert_eq!(
            effective_class(Some(&spec(ProtoClass::Read)), READ),
            EffectClass::Read
        );
    }

    #[test]
    fn string_form_round_trips() {
        for c in [
            EffectClass::Read,
            EffectClass::Idempotent,
            EffectClass::NonIdempotent,
        ] {
            assert_eq!(EffectClass::parse(c.as_str()), Some(c));
        }
        assert_eq!(EffectClass::parse("x"), None);
        assert_eq!(internal_adapter_class(), EffectClass::Idempotent);
    }
}
