package io.ctwo.internal;

import io.ctwo.CallContext;
import io.ctwo.Held;
import io.ctwo.MethodDefinition;
import io.ctwo.MethodMode;
import io.ctwo.ServerStream;
import io.ctwo.generated.GeneratedMethod;
import io.ctwo.generated.StreamServiceHandler;
import io.ctwo.generated.UnaryCodec;
import io.fastdb.payload.BuildPlan;
import io.fastdb.payload.Payload;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.Optional;
import java.util.function.Supplier;
import java.util.Objects;

/** JNI upcall adapter for generated resource bindings. */
public final class ServiceCallbacks {
    private static final ThreadLocal<CallContext> CURRENT = new ThreadLocal<>();

    private record Binding(GeneratedMethod method, UnaryCodec unary, StreamServiceHandler stream) {}
    private final Map<String, Binding> bindings;

    public ServiceCallbacks(Map<GeneratedMethod, UnaryCodec> unary,
            Map<GeneratedMethod, StreamServiceHandler> streams) {
        Map<String, Binding> entries = new HashMap<>();
        unary.forEach((method, handler) -> add(entries, method, Objects.requireNonNull(handler), null));
        streams.forEach((method, handler) -> add(entries, method, null, Objects.requireNonNull(handler)));
        bindings = Map.copyOf(entries);
    }

    public static Optional<CallContext> currentCallContext() {
        return Optional.ofNullable(CURRENT.get());
    }

    private static <T> T withinCallback(CallContext context, Supplier<T> callback) {
        CallContext previous = CURRENT.get();
        CURRENT.set(context);
        try {
            return callback.get();
        } finally {
            if (previous == null) {
                CURRENT.remove();
            } else {
                CURRENT.set(previous);
            }
        }
    }

    public void verifyBindings(List<MethodDefinition> methods) {
        if (methods.size() != bindings.size()) {
            throw new IllegalArgumentException("Service bindings must cover every method in the release");
        }
        for (var method : methods) {
            Binding binding = bindings.get(method.name());
            if (binding == null || (binding.stream() != null) != (method.mode() == MethodMode.SERVER_STREAM)) {
                throw new IllegalArgumentException("Binding does not match release method " + method.name());
            }
        }
    }

    private static void add(Map<String, Binding> entries, GeneratedMethod method,
            UnaryCodec unary, StreamServiceHandler stream) {
        if (entries.putIfAbsent(method.name(), new Binding(method, unary, stream)) != null) {
            throw new IllegalArgumentException("Duplicate method " + method.name());
        }
    }

    private Binding binding(CallContext context) {
        return Objects.requireNonNull(bindings.get(context.methodName()), "Missing generated method binding");
    }

    private Held<Payload> openInput(CallContext context, long id) {
        return PayloadBridge.held(binding(context).method().inputSpec(), id);
    }

    private Object invoke(CallContext context, Held<Payload> input) {
        return withinCallback(context, () -> binding(context).unary().invoke(input.value()));
    }

    private ActiveStream openStream(CallContext context, Held<Payload> input) {
        ServerStream<Payload> producer = withinCallback(context,
                () -> Objects.requireNonNull(binding(context).stream().open(input.value())));
        return new ActiveStream(context, input, producer, binding(context).method());
    }

    private Object encodeOutput(CallContext context, Object output) {
        var spec = binding(context).method().outputSpec();
        if (spec == null && output != null) {
            throw new IllegalArgumentException("Void method produced a payload");
        }
        return PayloadBridge.encoding(spec, output);
    }

    private static void closeOutput(Held<Payload> input, Object output) {
        if (output instanceof Payload payload && output != input.value()) {
            payload.close();
        } else if (output instanceof BuildPlan plan) {
            plan.close();
        }
    }

    private static final class ActiveStream {
        private final CallContext context;
        private final Held<Payload> input;
        private final ServerStream<Payload> producer;
        private final GeneratedMethod method;

        private ActiveStream(CallContext context, Held<Payload> input, ServerStream<Payload> producer,
                GeneratedMethod method) {
            this.context = context;
            this.input = input;
            this.producer = producer;
            this.method = method;
        }

        private Payload next() {
            return withinCallback(context, producer::next);
        }

        private Object encodeItem(Payload item) {
            return PayloadBridge.encoding(method.outputSpec(), item);
        }

        private void closeItem(Payload item) {
            closeOutput(input, item);
        }

        private void finish() {
            try (input) {
                withinCallback(context, () -> {
                    producer.close();
                    return null;
                });
            }
        }
    }
}
