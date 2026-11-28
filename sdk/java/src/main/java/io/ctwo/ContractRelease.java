package io.ctwo;

import io.ctwo.internal.NativeHandle;
import io.ctwo.internal.NativeLibraryLoader;
import java.nio.charset.StandardCharsets;
import java.util.List;
import java.util.Objects;

/** A portable contract validated and fingerprinted by Core. */
public final class ContractRelease {
    static {
        NativeLibraryLoader.load();
    }

    private final NativeHandle handle;

    private ContractRelease(long id) {
        handle = new NativeHandle(id);
    }

    public static ContractRelease fromDescriptorJson(byte[] json) {
        return new ContractRelease(nativeFromDescriptor(Objects.requireNonNull(json)));
    }

    public static ContractRelease fromDescriptorJson(String json) {
        return fromDescriptorJson(Objects.requireNonNull(json).getBytes(StandardCharsets.UTF_8));
    }

    public String canonicalJson() {
        return nativeDescriptor(handle.id());
    }

    public String descriptorSha256() {
        return nativeDigest(handle.id());
    }

    public String abiHash() {
        return nativeFingerprint(handle.id(), false);
    }

    public String signatureHash() {
        return nativeFingerprint(handle.id(), true);
    }

    public String releaseRefJson() {
        return nativeReference(handle.id());
    }

    public void verifyReleaseReference(String json) {
        nativeVerifyReference(handle.id(), Objects.requireNonNull(json).getBytes(StandardCharsets.UTF_8));
    }

    public ExpectedRouteContract expectedRoute(String routeName) {
        return nativeExpectedRoute(handle.id(), Objects.requireNonNull(routeName));
    }

    public List<MethodDefinition> methods() {
        return List.of(nativeMethods(handle.id()));
    }

    public List<String> topicNames() {
        return List.of(nativeTopicNames(handle.id()));
    }

    long nativeId() {
        return handle.id();
    }

    private static native long nativeFromDescriptor(byte[] json);
    private native String nativeDescriptor(long id);
    private native String nativeDigest(long id);
    private native String nativeFingerprint(long id, boolean signature);
    private native String nativeReference(long id);
    private native void nativeVerifyReference(long id, byte[] json);
    private native ExpectedRouteContract nativeExpectedRoute(long id, String routeName);
    private native MethodDefinition[] nativeMethods(long id);
    private native String[] nativeTopicNames(long id);
}
