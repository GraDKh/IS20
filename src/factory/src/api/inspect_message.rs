use crate::state::State;
use ic_cdk_macros::inspect_message;
use ic_storage::IcStorage;

#[cfg(not(feature = "no_api"))]
#[inspect_message]
fn inspect_message() {
    let state = State::get();
    let state = state.borrow();
    let factory = ic_factory::FactoryState::get();
    let factory = factory.borrow();

    if ic_cdk::api::call::method_name() == "set_token_bytecode"
        && factory.controller() == ic_canister::ic_kit::ic::caller()
    {
        return ic_cdk::api::call::accept_message();
    }

    match state.token_wasm {
        Some(_) => ic_cdk::api::call::accept_message(),
        None => ic_cdk::api::call::reject("the factory hasn't been completely intialized yet"),
    }
}
