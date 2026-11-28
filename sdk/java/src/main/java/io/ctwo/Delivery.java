package io.ctwo;

/** A delivered topic item and the count skipped before it. */
public record Delivery<T>(long seq, long dropped, T value) {}
