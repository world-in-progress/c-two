use c2_codegen::{ContractCodegenTarget, compile_contract_artifacts};
use c2_contract::ContractRelease;
use std::io::{self, Write};

fn main() {
    let descriptor =
        include_str!("../../../../tests/fixtures/contracts/portable-no-payload.contract.json");
    let release =
        ContractRelease::from_descriptor_json(descriptor.as_bytes()).expect("valid contract");
    let set = compile_contract_artifacts(
        &release,
        ContractCodegenTarget::TypeScript,
        &Default::default(),
    )
    .expect("compile TypeScript contract");
    let generated_contract = std::str::from_utf8(
        set.get("typescript/c_two_contract.ts")
            .expect("generated c_two_contract.ts")
            .bytes(),
    )
    .expect("UTF-8 TypeScript contract");
    let start = generated_contract
        .find("export interface C2ContractIdentity {")
        .expect("transport section start");
    let (transport, _) = generated_contract[start..]
        .split_once("\n\nexport interface C2FastDbCauseFields")
        .expect("transport section end");
    io::stdout()
        .write_all(transport.as_bytes())
        .expect("write rendered transport");
}
