//! Exports the build info (`VAM_VERSION` and friends); git is read here, not in vam-spec.

#[path = "../../crates/vam-spec/build_info_logic.rs"]
mod build_info_logic;

fn main() {
    build_info_logic::emit_build_info();
}
