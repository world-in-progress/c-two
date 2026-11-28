package io.ctwo;

import io.ctwo.error.C2Exception;
import io.ctwo.generated.GeneratedMethod;
import io.ctwo.generated.GeneratedTopic;
import io.ctwo.internal.NativeCalls;
import io.ctwo.internal.NativeHandle;
import io.ctwo.internal.PayloadBridge;
import io.fastdb.payload.CompiledSpec;
import io.fastdb.payload.FastDbException;
import io.fastdb.payload.Payload;
import java.util.Objects;

/** A Core route-bound client with immutable default call options. */
public final class Client implements AutoCloseable {
    private final NativeHandle handle;
    private final ExpectedRouteContract expected;
    private final CallOptions options;

    Client(long id, ExpectedRouteContract expected, CallOptions options) {
        this.handle = new NativeHandle(id);
        this.expected = expected;
        this.options = options;
    }

    public Client withCallOptions(CallOptions defaults) {
        long id = NativeCalls.withCallOptions(handle.id(), Objects.requireNonNull(defaults));
        return new Client(id, expected, defaults);
    }

    public CallOptions callOptions() { return options; }
    public ExpectedRouteContract expectedRoute() { return expected; }

    public CallOutcome queryCall(String callId) {
        return NativeCalls.query(handle.id(), Objects.requireNonNull(callId));
    }

    /** Invoked by generated unary clients. */
    public Payload call(GeneratedMethod method, String callId, Object input) {
        long result = NativeCalls.call(handle.id(), method.name(), callId, encode(method, input), false, false);
        return decode(method.outputSpec(), result);
    }

    /** Invoked by generated retained unary clients. */
    public Held<Payload> hold(GeneratedMethod method, String callId, Object input) {
        long result = NativeCalls.call(handle.id(), method.name(), callId, encode(method, input), false, true);
        return decodeHeld(method.outputSpec(), result);
    }

    public Payload recover(GeneratedMethod method, String callId) {
        long result = NativeCalls.recover(handle.id(), method.name(), Objects.requireNonNull(callId), false);
        return decode(method.outputSpec(), result);
    }

    public Held<Payload> holdRecover(GeneratedMethod method, String callId) {
        long result = NativeCalls.recover(handle.id(), method.name(), Objects.requireNonNull(callId), true);
        return decodeHeld(method.outputSpec(), result);
    }

    public ResponseStream<Payload> stream(GeneratedMethod method, Object input) {
        long stream = NativeCalls.call(handle.id(), method.name(), null, encode(method, input), true, false);
        return new ResponseStream<>(stream, id -> decode(method.outputSpec(), id),
                id -> decodeHeld(method.outputSpec(), id));
    }

    public Subscription<Payload> subscribe(GeneratedTopic topic, SubscribeOptions options) {
        long subscription = NativeCalls.subscribe(handle.id(), topic.name(),
                options.start().ordinal(), options.lagPolicy().ordinal());
        return new Subscription<>(subscription, topic.name(), id -> decode(topic.spec(), id),
                id -> decodeHeld(topic.spec(), id));
    }

    private static Object encode(GeneratedMethod method, Object input) {
        if (method.inputSpec() == null && input != null) {
            throw new IllegalArgumentException("Method does not accept a payload");
        }
        return PayloadBridge.encoding(method.inputSpec(), input);
    }

    private static Payload decode(CompiledSpec spec, long id) {
        try {
            return PayloadBridge.open(spec, id);
        } catch (FastDbException failure) {
            throw decodeFailure(failure);
        }
    }

    private static Held<Payload> decodeHeld(CompiledSpec spec, long id) {
        try {
            return PayloadBridge.held(spec, id);
        } catch (FastDbException failure) {
            throw decodeFailure(failure);
        }
    }

    private static C2Exception decodeFailure(FastDbException failure) {
        return NativeCalls.decodeFailure(failure);
    }

    @Override
    public void close() {
        handle.close();
    }
}
