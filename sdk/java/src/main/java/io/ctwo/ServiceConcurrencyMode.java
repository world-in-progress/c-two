package io.ctwo;

/** Execution concurrency policy for methods of a single registered resource. */
public enum ServiceConcurrencyMode {
    PARALLEL, EXCLUSIVE, READ_PARALLEL
}
