package io.ctwo;

import java.util.List;

/** Read-only Core memory-domain accounting for both Runtime roles and local holds. */
public record MemoryStatsSnapshot(
        Role client, Role server, List<RetiredRole> retired, LocalCopies holds) {

    public MemoryStatsSnapshot {
        retired = List.copyOf(retired);
    }

    private MemoryStatsSnapshot(Role client, Role server, RetiredRole[] retired, LocalCopies holds) {
        this(client, server, List.of(retired), holds);
    }

    public record Role(Long domainId, long generation, boolean frozen, boolean closed,
                       Cell memory, Cell fileSpill) {}

    public record Cell(long limitBytes, long usedBytes, long peakBytes,
                       long rejectedAllocations, long rejectedBytes) {}

    public record RetiredRole(String role, Role stats) {}

    public record LocalCopies(long activeHolds, long totalHeldBytes, double oldestHoldSeconds,
                              List<CopyBucket> byDirection) {
        public LocalCopies {
            byDirection = List.copyOf(byDirection);
        }

        private LocalCopies(long activeHolds, long totalHeldBytes, double oldestHoldSeconds,
                            CopyBucket[] byDirection) {
            this(activeHolds, totalHeldBytes, oldestHoldSeconds, List.of(byDirection));
        }
    }

    public record CopyBucket(String direction, long activeHolds, long totalHeldBytes) {}
}
