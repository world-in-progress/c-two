def local_server(address: str, *, ipc_overrides=None):
    """Create a server owned by one native session without relay discovery."""
    from c_two._native import RuntimeSession

    return RuntimeSession(
        server_id=address.removeprefix('ipc://'),
        server_ipc_overrides=ipc_overrides,
        use_process_relay_anchor=False,
    ).ensure_server_bridge()
