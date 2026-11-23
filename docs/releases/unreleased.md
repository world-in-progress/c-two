# Unreleased

Changes since [C-Two 0.12.0 / c3 0.8.0](0.12.0.md) (`f388cc1405ccf36741239ddda6bdeb6ee2e1d408`).

- ADR 0006 adds registration-scoped deduplication and recovery for unary calls with an explicit call ID. Python exposes `cc.with_call_id`, `cc.recover_call` and `cc.query_call`. Codes 721 `CallIdConflict`, 722 `CallResultUnavailable` and 723 `CallOutcomeUnknown` distinguish a conflicting method or input, a confirmed success without a recoverable result and a recovery miss. [ADR 0006](../decisions/0006-post-dispatch-outcome-and-dedup.md), [Python usage](../reference/python-usage.md), [errors](../reference/errors.md)
- IPC 18 adds keyed submission, recovery and an independent query record pair. C/FFI ABI 10 exposes these operations to Node and TypeScript. Upgrade communicating IPC peers and native bindings together, and regenerate TypeScript clients. SHM 4 and relay mesh 7 remain in use. [IPC handshake](../reference/ipc-handshake.md), [C ABI](../reference/c-abi.md)
- `HostOptions` sets the outcome entry limit and retained byte budget with `outcome_max_entries` and `outcome_max_retained_bytes`. `c3 relay` serves keyed submission, recovery and outcome queries at `/_c2/calls/keyed`, `/_c2/calls/recover` and `/_c2/calls/outcome`. [Configuration](../reference/configuration.md), [relay routing](../reference/contracts-and-routing.md#unary-call-outcomes-over-http)
