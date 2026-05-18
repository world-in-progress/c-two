use c2_codegen::{TypeScriptOptions, generate_typescript_client};

fn control_contract_json() -> String {
    r#"{
      "schema": "c-two.contract.v1",
      "crm": {
        "namespace": "test.codegen",
        "name": "Portable",
        "version": "0.1.0"
      },
      "methods": [
        {
          "access": "read",
          "buffer": "view",
          "name": "echo",
          "parameters": [
            {
              "name": "value",
              "kind": "POSITIONAL_OR_KEYWORD",
              "default": {"kind": "missing"},
              "type": {"kind": "primitive", "name": "str"}
            },
            {
              "name": "suffix",
              "kind": "POSITIONAL_OR_KEYWORD",
              "default": {"kind": "json_scalar", "value": null},
              "type": {
                "kind": "union",
                "items": [
                  {"kind": "none"},
                  {"kind": "primitive", "name": "str"}
                ]
              }
            }
          ],
          "return": {"kind": "primitive", "name": "str"},
          "wire": {
            "input": {
              "kind": "codec_ref",
              "id": "c-two.control.json",
              "version": "1",
              "schema": "c-two.control.json.v1",
              "portable": true
            },
            "output": {
              "kind": "codec_ref",
              "id": "c-two.control.json",
              "version": "1",
              "schema": "c-two.control.json.v1",
              "portable": true
            }
          }
        }
      ]
    }"#
    .to_string()
}

fn external_codec_contract_json() -> String {
    r#"{
      "schema": "c-two.contract.v1",
      "crm": {
        "namespace": "test.codegen",
        "name": "Portable",
        "version": "0.1.0"
      },
      "methods": [
        {
          "access": "write",
          "buffer": "view",
          "name": "load_payload",
          "parameters": [],
          "return": {
            "kind": "codec",
            "codec": {
              "kind": "codec_ref",
              "id": "org.example.payload",
              "version": "1",
              "schema_sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
              "portable": true
            }
          },
          "wire": {
            "input": null,
            "output": {
              "kind": "codec_ref",
              "id": "org.example.payload",
              "version": "1",
              "schema_sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
              "portable": true
            }
          }
        }
      ]
    }"#
    .to_string()
}

fn name_collision_contract_json() -> String {
    r#"{
      "schema": "c-two.contract.v1",
      "crm": {
        "namespace": "test.codegen",
        "name": "Portable",
        "version": "0.1.0"
      },
      "methods": [
        {
          "access": "write",
          "buffer": "view",
          "name": "foo-bar",
          "parameters": [],
          "return": {"kind": "none"},
          "wire": {"input": null, "output": null}
        },
        {
          "access": "write",
          "buffer": "view",
          "name": "foo_bar",
          "parameters": [],
          "return": {"kind": "none"},
          "wire": {"input": null, "output": null}
        }
      ]
    }"#
    .to_string()
}

fn parameter_collision_contract_json() -> String {
    r#"{
      "schema": "c-two.contract.v1",
      "crm": {
        "namespace": "test.codegen",
        "name": "Portable",
        "version": "0.1.0"
      },
      "methods": [
        {
          "access": "write",
          "buffer": "view",
          "name": "echo",
          "parameters": [
            {
              "name": "value-id",
              "kind": "POSITIONAL_OR_KEYWORD",
              "default": {"kind": "missing"},
              "type": {"kind": "primitive", "name": "str"}
            },
            {
              "name": "value_id",
              "kind": "POSITIONAL_OR_KEYWORD",
              "default": {"kind": "missing"},
              "type": {"kind": "primitive", "name": "str"}
            }
          ],
          "return": {"kind": "none"},
          "wire": {
            "input": {
              "kind": "codec_ref",
              "id": "c-two.control.json",
              "version": "1",
              "schema": "c-two.control.json.v1",
              "portable": true
            },
            "output": null
          }
        }
      ]
    }"#
    .to_string()
}

#[test]
fn generates_typescript_client_for_control_contract() {
    let output = generate_typescript_client(
        control_contract_json().as_bytes(),
        TypeScriptOptions::default(),
    )
    .unwrap();

    assert!(output.contains("export const PORTABLE_CONTRACT"));
    assert!(output.contains("export class PortableClient"));
    assert!(output.contains("async echo(value: string, suffix?: null | string): Promise<string>"));
    assert!(output.contains("compactTrailingUndefined([value, suffix])"));
    assert!(
        output.contains(
            "export const PORTABLE_CODEC_REQUIREMENTS: readonly C2CodecRequirement[] = [];"
        )
    );
}

#[test]
fn generates_opaque_types_and_requirements_for_external_codecs() {
    let output = generate_typescript_client(
        external_codec_contract_json().as_bytes(),
        TypeScriptOptions::default(),
    )
    .unwrap();

    assert!(output.contains("Promise<C2CodecValue<\"org.example.payload\">>"));
    assert!(
        output.contains(
            "export const PORTABLE_CODEC_REQUIREMENTS: readonly C2CodecRequirement[] = ["
        )
    );
    assert!(output.contains("id: \"org.example.payload\""));
    assert!(output.contains(
        "schemaSha256: \"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\""
    ));
    assert!(output.contains("export interface C2CodecImplementation"));
    assert!(output.contains("export function createCodecStubs(): C2CodecRegistry"));
    assert!(output.contains("Codec ${requirement.id} requires application runtime implementation"));
}

#[test]
fn strict_codec_mode_rejects_external_codecs() {
    let err = generate_typescript_client(
        external_codec_contract_json().as_bytes(),
        TypeScriptOptions {
            strict_codecs: true,
        },
    )
    .unwrap_err();

    assert!(
        err.to_string()
            .contains("unsupported codec org.example.payload")
    );
}

#[test]
fn rejects_invalid_contract_descriptor() {
    let err = generate_typescript_client(br#"{"schema":"wrong"}"#, TypeScriptOptions::default())
        .unwrap_err();

    assert!(err.to_string().contains("contract descriptor invalid"));
}

#[test]
fn rejects_typescript_method_identifier_collisions() {
    let err = generate_typescript_client(
        name_collision_contract_json().as_bytes(),
        TypeScriptOptions::default(),
    )
    .unwrap_err();

    assert!(err.to_string().contains("method name collision"));
}

#[test]
fn rejects_typescript_parameter_identifier_collisions() {
    let err = generate_typescript_client(
        parameter_collision_contract_json().as_bytes(),
        TypeScriptOptions::default(),
    )
    .unwrap_err();

    assert!(err.to_string().contains("parameter name collision"));
}
