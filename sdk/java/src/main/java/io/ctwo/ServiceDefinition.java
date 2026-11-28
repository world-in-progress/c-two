package io.ctwo;

import io.ctwo.internal.ServiceCallbacks;
import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.Map;
import java.util.Objects;

/** A release, callback adapter and per-registration configuration for one route. */
public final class ServiceDefinition {
    private final ContractRelease release;
    private final String routeName;
    private final ServiceCallbacks callbacks;
    private final ServiceConcurrencyMode concurrencyMode;
    private final Integer maxPending;
    private final Integer maxWorkers;
    private final Map<String, Integer> capacities;
    private final Map<String, byte[]> initialBytes;

    public ServiceDefinition(ContractRelease release, String routeName, ServiceCallbacks callbacks) {
        this(Objects.requireNonNull(release), Objects.requireNonNull(routeName),
                Objects.requireNonNull(callbacks), ServiceConcurrencyMode.READ_PARALLEL,
                null, null, Map.of(), Map.of());
    }

    private ServiceDefinition(
            ContractRelease release, String routeName, ServiceCallbacks callbacks,
            ServiceConcurrencyMode concurrencyMode, Integer maxPending, Integer maxWorkers,
            Map<String, Integer> capacities, Map<String, byte[]> initialBytes) {
        this.release = release;
        this.routeName = routeName;
        this.callbacks = callbacks;
        this.concurrencyMode = concurrencyMode;
        this.maxPending = maxPending;
        this.maxWorkers = maxWorkers;
        this.capacities = capacities;
        this.initialBytes = initialBytes;
    }

    public ContractRelease release() {
        return release;
    }

    public String routeName() {
        return routeName;
    }

    public ServiceDefinition withConcurrency(
            ServiceConcurrencyMode mode, Integer maxPending, Integer maxWorkers) {
        return new ServiceDefinition(release, routeName, callbacks, Objects.requireNonNull(mode),
                maxPending, maxWorkers, capacities, initialBytes);
    }

    public ServiceDefinition withTopicCapacity(String topicName, int capacity) {
        var updated = new LinkedHashMap<>(capacities);
        updated.put(Objects.requireNonNull(topicName), capacity);
        return new ServiceDefinition(release, routeName, callbacks, concurrencyMode,
                maxPending, maxWorkers, Collections.unmodifiableMap(updated), initialBytes);
    }

    /** Stage a copy of the encoded payload for publication before the route opens. */
    public ServiceDefinition withInitialTopicBytes(String topicName, byte[] payload) {
        var updated = new LinkedHashMap<>(initialBytes);
        if (updated.putIfAbsent(Objects.requireNonNull(topicName), payload.clone()) != null) {
            throw new IllegalArgumentException("Duplicate initial payload for topic " + topicName);
        }
        return new ServiceDefinition(release, routeName, callbacks, concurrencyMode,
                maxPending, maxWorkers, capacities, Collections.unmodifiableMap(updated));
    }

    ServiceCallbacks callbacks() { return callbacks; }
    int nativeConcurrencyMode() { return concurrencyMode.ordinal(); }
    Integer maxPending() { return maxPending; }
    Integer maxWorkers() { return maxWorkers; }
    String[] capacityTopics() { return capacities.keySet().toArray(String[]::new); }
    int[] topicCapacities() { return capacities.values().stream().mapToInt(Integer::intValue).toArray(); }
    String[] initialTopics() { return initialBytes.keySet().toArray(String[]::new); }
    byte[][] initialPayloads() { return initialBytes.values().toArray(byte[][]::new); }
}
