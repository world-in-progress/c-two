package io.ctwo.internal;

import io.ctwo.CallOptions;
import io.ctwo.CallOutcome;
import io.ctwo.StreamCloseReceipt;
import io.ctwo.TopicCloseReceipt;
import io.ctwo.error.C2Exception;

/** JNI declarations for calls, streams, subscriptions and FastDB backing projections. */
public final class NativeCalls {
    static {
        NativeLibraryLoader.load();
    }

    private NativeCalls() {}

    public static native long withCallOptions(long client, CallOptions options);
    public static native long call(long client, String method, String callId, Object payload,
            boolean stream, boolean held);
    public static native long recover(long client, String method, String callId, boolean held);
    public static native C2Exception decodeFailure(Throwable failure);
    public static native CallOutcome query(long client, String callId);
    public static native long streamNext(long stream, boolean held);
    public static native void streamCancel(long stream);
    public static native StreamCloseReceipt streamClose(long stream, long seconds, int nanos);
    public static native long subscribe(long client, String topic, int start, int lagPolicy);
    public static native long subscriptionStartSeq(long subscription);
    public static native long[] topicNext(long subscription, boolean held);
    public static native void topicCancel(long subscription);
    public static native TopicCloseReceipt topicClose(long subscription, long seconds, int nanos);
    public static native long publish(long publisher, Object payload);
    public static native long publisherHeadSeq(long publisher);
    public static native boolean publisherIsClosed(long publisher);
    static native long[] backing(long owner);
    public static native long[] directSlabCounts();
}
