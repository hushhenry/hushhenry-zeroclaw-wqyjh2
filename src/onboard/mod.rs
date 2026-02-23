pub mod channels;
pub mod tui;
pub mod wizard;

pub use tui::{run_channels_repair_tui, run_wizard_tui};
pub use wizard::{
    run_models_refresh, run_quick_setup, scaffold_agent_workspace,
};

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_reexport_exists<F>(_value: F) {}

    #[test]
    fn wizard_functions_are_reexported() {
        assert_reexport_exists(run_wizard_tui);
        assert_reexport_exists(run_channels_repair_tui);
        assert_reexport_exists(run_quick_setup);
        assert_reexport_exists(run_models_refresh);
    }
}
