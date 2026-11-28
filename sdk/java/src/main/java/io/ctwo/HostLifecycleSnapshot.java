package io.ctwo;

/** Read-only Host lifecycle state. Work drain is unknown until Core reports route outcomes. */
public record HostLifecycleSnapshot(boolean finished, boolean listenerClosed, Boolean workDrained) {}
