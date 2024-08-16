use emulated_integration_tests_common::impls::TestExt;
use rococo_westend_system_emulated_network::{PenpalAPara};
// use penpal_emulated_chain::{PenpalAParaPallet};

#[test]
fn sandbox() {
    PenpalAPara::execute_with(|| {
    });
}