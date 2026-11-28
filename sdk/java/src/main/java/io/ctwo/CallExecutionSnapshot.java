package io.ctwo;

import java.util.List;

/** Read-only call-admission slot accounting. Dormant roles have absent counters. */
public record CallExecutionSnapshot(Role client, Role server, List<RetiredRole> retired) {

    public CallExecutionSnapshot {
        retired = List.copyOf(retired);
    }

    private CallExecutionSnapshot(Role client, Role server, RetiredRole[] retired) {
        this(client, server, List.of(retired));
    }

    public record Role(Long maxOperations, Long usedOperations, Long peakOperations,
                       Long rejectedReservations, boolean closed) {}

    public record RetiredRole(String role, Role stats) {}
}
