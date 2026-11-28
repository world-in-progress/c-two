package io.ctwo.generated;

import io.ctwo.Client;
import io.ctwo.ContractRelease;
import io.ctwo.Held;
import io.ctwo.ResponseStream;
import io.ctwo.ServiceDefinition;
import io.ctwo.SubscribeOptions;
import io.ctwo.Subscription;
import io.ctwo.internal.ServiceCallbacks;
import io.fastdb.payload.BuildPlan;
import io.fastdb.payload.Payload;
import java.util.Map;
import java.util.Objects;

/** Wire-free adapter methods used by code generated for a portable contract release. */
public final class GeneratedContractSupport {
    private GeneratedContractSupport() {}

    public static ServiceDefinition service(ContractRelease release, String routeName,
            Map<GeneratedMethod, UnaryCodec> unary, Map<GeneratedMethod, StreamServiceHandler> streams) {
        var callbacks = new ServiceCallbacks(unary, streams);
        callbacks.verifyBindings(release.methods());
        return new ServiceDefinition(release, routeName, callbacks);
    }

    public static Payload call(Client client, GeneratedMethod method, Payload input) {
        return client.call(method, null, input);
    }

    public static Payload call(Client client, GeneratedMethod method, BuildPlan input) {
        return client.call(method, null, input);
    }

    public static Payload call(Client client, GeneratedMethod method) {
        return client.call(method, null, null);
    }

    public static Held<Payload> hold(Client client, GeneratedMethod method, Payload input) {
        return client.hold(method, null, input);
    }

    public static Held<Payload> hold(Client client, GeneratedMethod method, BuildPlan input) {
        return client.hold(method, null, input);
    }

    public static Held<Payload> hold(Client client, GeneratedMethod method) {
        return client.hold(method, null, null);
    }

    public static Payload callKeyed(Client client, GeneratedMethod method, String callId) {
        return client.call(method, Objects.requireNonNull(callId), null);
    }

    public static Payload callKeyed(Client client, GeneratedMethod method, String callId, Payload input) {
        return client.call(method, Objects.requireNonNull(callId), input);
    }

    public static Payload callKeyed(Client client, GeneratedMethod method, String callId, BuildPlan input) {
        return client.call(method, Objects.requireNonNull(callId), input);
    }

    public static Held<Payload> holdKeyed(Client client, GeneratedMethod method, String callId) {
        return client.hold(method, Objects.requireNonNull(callId), null);
    }

    public static Held<Payload> holdKeyed(Client client, GeneratedMethod method, String callId, Payload input) {
        return client.hold(method, Objects.requireNonNull(callId), input);
    }

    public static Held<Payload> holdKeyed(Client client, GeneratedMethod method, String callId, BuildPlan input) {
        return client.hold(method, Objects.requireNonNull(callId), input);
    }

    public static Payload recover(Client client, GeneratedMethod method, String callId) {
        return client.recover(method, callId);
    }

    public static Held<Payload> holdRecover(Client client, GeneratedMethod method, String callId) {
        return client.holdRecover(method, callId);
    }

    public static ResponseStream<Payload> stream(Client client, GeneratedMethod method) {
        return client.stream(method, null);
    }

    public static ResponseStream<Payload> stream(Client client, GeneratedMethod method, Payload input) {
        return client.stream(method, input);
    }

    public static ResponseStream<Payload> stream(Client client, GeneratedMethod method, BuildPlan input) {
        return client.stream(method, input);
    }

    public static Subscription<Payload> subscribe(Client client, GeneratedTopic topic, SubscribeOptions options) {
        return client.subscribe(topic, options);
    }
}
