package io.ctwo;

import java.util.List;

/** Core's shutdown outcome as observed within a caller's wait budget. */
public record ShutdownOutcome(
        boolean completed, List<String> removedRoutes, List<RouteClose> routeOutcomes,
        List<RelayCleanup> relayErrors, boolean serverWasStarted, boolean ipcClientsDrained,
        String routeCloseError, String runtimeBarrierError, String ipcClientCloseError,
        List<IpcClientClose> ipcClientCloseDetails) {

    public ShutdownOutcome {
        removedRoutes = List.copyOf(removedRoutes);
        routeOutcomes = List.copyOf(routeOutcomes);
        relayErrors = List.copyOf(relayErrors);
        ipcClientCloseDetails = List.copyOf(ipcClientCloseDetails);
    }

    private ShutdownOutcome(boolean completed, String[] removedRoutes, RouteClose[] routeOutcomes,
                             RelayCleanup[] relayErrors, boolean serverWasStarted, boolean ipcClientsDrained,
                             String routeCloseError, String runtimeBarrierError, String ipcClientCloseError,
                             IpcClientClose[] ipcClientCloseDetails) {
        this(completed, List.of(removedRoutes), List.of(routeOutcomes), List.of(relayErrors),
                serverWasStarted, ipcClientsDrained, routeCloseError, runtimeBarrierError, ipcClientCloseError,
                List.of(ipcClientCloseDetails));
    }

    public record RouteClose(String routeName, boolean localRemoved, boolean activeDrained,
                             String closedReason, String closeError) {}

    public record RelayCleanup(String routeName, Integer statusCode, String message) {}

    public record IpcClientClose(String address, String reason) {}
}
