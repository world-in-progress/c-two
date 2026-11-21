"""Resource-owned publish/subscribe example (ADR 0005).

Demonstrates:
  - @cc.topic on a CRM contract (zero input, one published output)
  - cc.register() returning a Registration with a registration-scoped publisher
  - Natural topic calls: telemetry.state() / telemetry.heartbeat(start='next')
  - cc.hold(telemetry.state)() retaining deliveries as HeldResult owners
  - cc.with_call_options(...) views carrying the subscription policy
  - Publisher refusal with ResourceClosed after the registration ends

Run:
    uv run python examples/python/pubsub.py
"""

import c_two as cc


# ── Define CRM contract with topics ────────────────────────────────

@cc.crm(namespace='demo.telemetry', version='0.1.0')
class Telemetry:
    @cc.read
    def device_name(self) -> str: ...

    @cc.topic
    def state(self) -> dict: ...

    @cc.topic
    def heartbeat(self) -> int: ...


# ── Define the resource (topics need no resource implementation) ───

class TelemetryImpl:
    def __init__(self, name: str = 'sensor-1'):
        self._name = name

    def device_name(self) -> str:
        return self._name


# ── Main ────────────────────────────────────────────────────────────

def main():
    # 1. Register with an initial state frame committed atomically before the
    #    route opens, and a deeper history ring for the heartbeat topic.
    registration = cc.register(
        Telemetry,
        TelemetryImpl(),
        name='telemetry',
        topic_capacity={'state': 4, 'heartbeat': 8},
        initial_topics={'state': {'online': True, 'voltage': 3.3}},
    )
    print(f'Registered route: {registration.name}')

    state_pub = registration.publisher('state')
    heartbeat_pub = registration.publisher('heartbeat')

    # 2. Connect and subscribe through the natural topic members.
    #    `latest` sees the initial frame first; `next` waits for frames
    #    published after attach. cc.hold(...) keeps a local copy per delivery.
    telemetry = cc.connect(Telemetry, name='telemetry')
    latest = telemetry.state()
    held_stream = cc.hold(telemetry.state)()
    following = cc.with_call_options(telemetry).heartbeat(start='next')
    print(f'state start_seq={latest.start_seq}, heartbeat start_seq={following.start_seq}')

    # 3. Publish immutable frames; slow subscribers never block the publisher.
    seq = state_pub.publish({'online': True, 'voltage': 3.1})
    print(f'published state seq={seq}')
    for beat in range(3):
        heartbeat_pub.publish(beat)

    delivery = next(latest)
    print(f'state delivery: value={delivery.value} seq={delivery.seq} dropped={delivery.dropped}')
    retained = next(held_stream)
    print(f'held delivery: value={retained.value.value} seq={retained.seq}')
    beats = [next(following).value for _ in range(3)]
    print(f'heartbeat deliveries: {beats}')

    # 4. Explicit close returns a truthful terminal receipt; the retained
    #    local copy stays valid afterwards and is released by its owner.
    latest.close(timeout=5.0)
    held_stream.close(timeout=5.0)
    following.close(timeout=5.0)
    print(f'held value after close: {retained.value.value}')
    retained.value.release()
    cc.close(telemetry)

    # 5. Unregistering the route ends every publisher of that registration.
    cc.unregister('telemetry')
    try:
        state_pub.publish({'online': False})
    except cc.error.ResourceClosed as exc:
        print(f'publisher after unregister: {type(exc).__name__} (code {exc.code})')

    cc.shutdown()
    print('done')


if __name__ == '__main__':
    main()
