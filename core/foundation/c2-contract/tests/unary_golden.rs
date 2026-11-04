use c2_contract::{ContractRelease, canonical_json};
use serde_json::{Value, json};

// The no-payload, record and object-graph fixtures are shared by the Python,
// Rust and TypeScript portable matrix. Pins were captured at ba231cdf.
#[test]
fn existing_unary_fixture_identities_remain_byte_identical() {
    for (source, digest) in [
        (
            include_str!("../../../../tests/fixtures/contracts/portable-release.contract.json"),
            "d25a8e308c803acd08df897279827384fc0de97ea1cacd3323554b94c6b560a3",
        ),
        (
            include_str!("../../../../tests/fixtures/contracts/portable-no-payload.contract.json"),
            "a1e05b1f83b46bbe1aec46b91e953fdd12daea6067279e3cf3735d919d59987e",
        ),
        (
            include_str!("../../../../tests/fixtures/contracts/portable-record-v1.contract.json"),
            "0eeab1f5cdc6bac8ae7880794dd1b2c1f8eaac34e8e63c9139d6c40a266ce10c",
        ),
        (
            include_str!(
                "../../../../tests/fixtures/contracts/portable-object-graph-v1.contract.json"
            ),
            "a60647161cc12058cc829fdbaa30f999ffad40888ef24172fe3d4dd86bb4c98d",
        ),
    ] {
        let value: Value = serde_json::from_str(source).unwrap();
        let release = ContractRelease::from_descriptor_json(source.as_bytes()).unwrap();
        let descriptor = release.descriptor();
        assert_eq!(descriptor.descriptor_sha256().as_str(), digest);
        assert_eq!(descriptor.canonical_json(), canonical_json(&value));
        assert_eq!(descriptor.abi_hash(), value["fingerprints"]["abi_hash"]);
        assert_eq!(
            descriptor.signature_hash(),
            value["fingerprints"]["signature_hash"]
        );
        assert_eq!(
            release.reference().to_canonical_json().unwrap(),
            canonical_json(&json!({
                "schema": "c-two.contract-release-ref.v1",
                "contract_schema": "c-two.contract.v2",
                "crm": value["crm"],
                "descriptor_sha256": digest,
            })),
        );
    }
}
