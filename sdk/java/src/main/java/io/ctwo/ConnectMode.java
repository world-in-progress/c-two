package io.ctwo;

import java.util.Objects;

/** Selects how Core acquires a route connection. */
public final class ConnectMode {
    public enum Kind { DIRECT_IPC, EXPLICIT_RELAY, RELAY_AWARE }

    private final Kind kind;
    private final String address;

    private ConnectMode(Kind kind, String address) {
        this.kind = kind;
        this.address = address;
    }

    public static ConnectMode directIpc(String address) {
        return new ConnectMode(Kind.DIRECT_IPC, Objects.requireNonNull(address));
    }

    public static ConnectMode explicitRelay(String address) {
        return new ConnectMode(Kind.EXPLICIT_RELAY, Objects.requireNonNull(address));
    }

    public static ConnectMode relayAware() {
        return new ConnectMode(Kind.RELAY_AWARE, null);
    }

    public static ConnectMode relayAwareAt(String address) {
        return new ConnectMode(Kind.RELAY_AWARE, Objects.requireNonNull(address));
    }

    public Kind kind() { return kind; }
    public String address() { return address; }
}
