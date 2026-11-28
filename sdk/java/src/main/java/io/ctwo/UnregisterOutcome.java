package io.ctwo;

/** Core's result of unregistering one route. */
public record UnregisterOutcome(String routeName, boolean localRemoved,
                                ShutdownOutcome.RouteClose close,
                                ShutdownOutcome.RelayCleanup relayError) {}
