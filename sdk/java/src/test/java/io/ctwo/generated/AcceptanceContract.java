package io.ctwo.generated;

import io.ctwo.Client;
import io.ctwo.ContractRelease;
import io.ctwo.Held;
import io.ctwo.Publisher;
import io.ctwo.Registration;
import io.ctwo.ResponseStream;
import io.ctwo.ServiceDefinition;
import io.ctwo.SubscribeOptions;
import io.ctwo.Subscription;
import io.fastdb.payload.BuildPlan;
import io.fastdb.payload.BuildPolicy;
import io.fastdb.payload.Builder;
import io.fastdb.payload.CompiledSpec;
import io.fastdb.payload.Payload;
import io.fastdb.payload.View;

import java.io.IOException;
import java.io.UncheckedIOException;
import java.util.Map;
import java.util.Objects;

/** Reference shape for Java contract code generation. */
final class AcceptanceContract {
    private static final String SPEC_JSON = """
            {
              "schema": "fastdb.payload.v1",
              "profile": "record.v1",
              "entries": [{"id": "value", "cardinality": "one", "type": {"kind": "bytes"}}],
              "components": []
            }
            """;

    static final ContractRelease RELEASE = loadRelease();
    static final CompiledSpec VALUE_SPEC = CompiledSpec.compile(SPEC_JSON);

    private static final GeneratedMethod PING = new GeneratedMethod("ping", null, null);
    private static final GeneratedMethod ECHO = new GeneratedMethod("echo", VALUE_SPEC, VALUE_SPEC);
    private static final GeneratedMethod SCAN = new GeneratedMethod("scan", VALUE_SPEC, VALUE_SPEC);
    private static final GeneratedTopic STATE = new GeneratedTopic("state", VALUE_SPEC);
    private static final GeneratedTopic TICKS = new GeneratedTopic("ticks", VALUE_SPEC);

    private AcceptanceContract() {}

    static ServiceDefinition service(String route, UnaryCodec echo, StreamServiceHandler scan) {
        return GeneratedContractSupport.service(RELEASE, route, Map.of(PING, input -> null, ECHO, echo),
                Map.of(SCAN, scan));
    }

    static byte[] bytes(Payload payload) {
        try (View entry = payload.entryView(0); View value = entry.at(0)) {
            return value.getBytes();
        }
    }

    static BuildPlan plan(byte[] bytes) {
        try (Builder builder = Builder.create(VALUE_SPEC)) {
            builder.entryBegin(0, 1).valueBytes(bytes);
            return builder.freeze();
        }
    }

    static Payload payload(byte[] bytes) {
        try (BuildPlan plan = plan(bytes)) {
            return plan.execute(BuildPolicy.ALLOW_STAGING).payload();
        }
    }

    static Publisher<Payload> state(Registration registration) {
        return registration.publisher("state", VALUE_SPEC);
    }

    static Publisher<Payload> ticks(Registration registration) {
        return registration.publisher("ticks", VALUE_SPEC);
    }

    static final class Stub {
        private final Client client;

        Stub(Client client) {
            this.client = client;
        }

        Payload ping() {
            return GeneratedContractSupport.call(client, PING);
        }

        Payload echo(Payload input) {
            return GeneratedContractSupport.call(client, ECHO, input);
        }

        Payload echo(BuildPlan input) {
            return GeneratedContractSupport.call(client, ECHO, input);
        }

        Held<Payload> holdEcho(Payload input) {
            return GeneratedContractSupport.hold(client, ECHO, input);
        }

        Payload echoKeyed(String callId, Payload input) {
            return GeneratedContractSupport.callKeyed(client, ECHO, callId, input);
        }

        Held<Payload> holdEchoKeyed(String callId, Payload input) {
            return GeneratedContractSupport.holdKeyed(client, ECHO, callId, input);
        }

        Payload recoverEcho(String callId) {
            return GeneratedContractSupport.recover(client, ECHO, callId);
        }

        Held<Payload> holdRecoverEcho(String callId) {
            return GeneratedContractSupport.holdRecover(client, ECHO, callId);
        }

        ResponseStream<Payload> scan(Payload input) {
            return GeneratedContractSupport.stream(client, SCAN, input);
        }

        Subscription<Payload> state(SubscribeOptions options) {
            return GeneratedContractSupport.subscribe(client, STATE, options);
        }

        Subscription<Payload> ticks(SubscribeOptions options) {
            return GeneratedContractSupport.subscribe(client, TICKS, options);
        }
    }

    private static ContractRelease loadRelease() {
        try (var source = Objects.requireNonNull(
                AcceptanceContract.class.getResourceAsStream("acceptance.contract.json"))) {
            return ContractRelease.fromDescriptorJson(source.readAllBytes());
        } catch (IOException error) {
            throw new UncheckedIOException(error);
        }
    }
}
