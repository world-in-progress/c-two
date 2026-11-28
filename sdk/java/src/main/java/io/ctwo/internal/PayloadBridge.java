package io.ctwo.internal;

import io.ctwo.Held;
import io.fastdb.payload.Access;
import io.fastdb.payload.BuildPlan;
import io.fastdb.payload.BuildPolicy;
import io.fastdb.payload.CompiledSpec;
import io.fastdb.payload.DirectBuildStatus;
import io.fastdb.payload.ExecutionMode;
import io.fastdb.payload.ExecutionResult;
import io.fastdb.payload.FastDbException;
import io.fastdb.payload.NativeBytesBacking;
import io.fastdb.payload.NativeWritableBacking;
import io.fastdb.payload.OpenOptions;
import io.fastdb.payload.Payload;
import java.lang.ref.Reference;

/** Projects Core-owned memory through the public FastDB backing interfaces. */
public final class PayloadBridge {
    private record Encoding(Object value, byte[] digest) {}

    private PayloadBridge() {}

    public static Object encoding(CompiledSpec spec, Object input) {
        return spec == null ? input : new Encoding(input, spec.sha256());
    }

    public static Payload open(CompiledSpec spec, long ownerId) {
        try (NativeHandle owner = new NativeHandle(ownerId)) {
            long[] backing = NativeCalls.backing(owner.id());
            if (spec == null) {
                if (backing[1] != 0) {
                    throw new IllegalArgumentException("Expected an empty payload");
                }
                return null;
            }
            OpenOptions options = OpenOptions.defaults();
            if (backing[1] > options.maxTotalBytes()) {
                options = options.withMaxTotalBytes(backing[1]);
            }
            try {
                return Payload.openExternalNative(spec,
                        new NativeBytesBacking(backing[0], backing[1], backing[2], backing[3], backing[4]), options);
            } finally {
                // FastDB's retain must run before the registry owner can be cleaned.
                Reference.reachabilityFence(owner);
            }
        }
    }

    public static Held<Payload> held(CompiledSpec spec, long ownerId) {
        Payload payload = open(spec, ownerId);
        if (payload == null) {
            return new Held<>(null, () -> {});
        }
        try {
            Payload invalidator = payload.retain();
            try {
                return new Held<>(payload, () -> {
                    try (invalidator; payload) {
                        invalidator.invalidate();
                    }
                });
            } catch (RuntimeException | Error failure) {
                invalidator.close();
                throw failure;
            }
        } catch (RuntimeException | Error failure) {
            payload.close();
            throw failure;
        }
    }

    public static void invalidate(Payload payload) {
        if (payload == null) {
            return;
        }
        try (payload) {
            payload.invalidate();
        }
    }

    static long planSize(Object input) {
        Object value = encodingValue(input);
        if (value instanceof BuildPlan plan) {
            return plan.info().totalBytes();
        }
        if (value instanceof Payload) {
            return -1;
        }
        throw new IllegalArgumentException("Expected a FastDB Payload or BuildPlan");
    }

    static boolean canDirect(Object input) {
        return encodingValue(input) instanceof BuildPlan plan
                && plan.info().directBuildStatus() == DirectBuildStatus.ELIGIBLE;
    }

    static boolean direct(Object input, long slabTableAddress) {
        BuildPlan plan = (BuildPlan) encodingValue(input);
        try (ExecutionResult result = plan.executeExternalNative(BuildPolicy.REQUIRE_DIRECT,
                new NativeWritableBacking(slabTableAddress))) {
            if (result.report().mode() != ExecutionMode.DIRECT || result.report().stagingBytes() != 0) {
                throw new IllegalStateException("FastDB did not build directly into the C-Two slab");
            }
            requireDigest(input, result.payload());
            return true;
        } catch (FastDbException failure) {
            if (failure.code() == FastDbException.E_DIRECT_UNAVAILABLE) {
                return false;
            }
            throw failure;
        }
    }

    static Access access(Object input) {
        Object value = encodingValue(input);
        if (value instanceof Payload payload) {
            requireDigest(input, payload);
            return payload.acquire();
        }
        if (value instanceof BuildPlan plan) {
            try (ExecutionResult result = plan.execute(BuildPolicy.ALLOW_STAGING)) {
                requireDigest(input, result.payload());
                return result.payload().acquire();
            }
        }
        throw new IllegalArgumentException("Expected a FastDB Payload or BuildPlan");
    }

    private static Object encodingValue(Object input) {
        return input instanceof Encoding encoding ? encoding.value() : input;
    }

    private static void requireDigest(Object input, Payload payload) {
        if (input instanceof Encoding encoding) {
            payload.requireSpecSha256(encoding.digest());
        }
    }
}
