package io.ctwo;

/** Synchronous Host admission policy. It must not call back into operations on the same Host. */
@FunctionalInterface
public interface AdmissionHook {
    AdmissionDecision admit(AdmissionContext context);
}
