def local_server(address: str, **host_options):
    """Create a server owned by one native session without relay discovery."""
    from c_two._native import RuntimeSession
    from c_two.transport.registry import Runtime

    runtime = Runtime()
    runtime._runtime_session = RuntimeSession(use_process_relay_anchor=False)
    runtime.set_server(server_id=address.removeprefix('ipc://'), **host_options)
    return runtime._runtime_session.ensure_server_bridge()
