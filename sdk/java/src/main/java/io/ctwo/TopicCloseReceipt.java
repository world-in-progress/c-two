package io.ctwo;

import io.ctwo.error.C2Exception;

/** Terminal observation or a receipt for an operation still closing. */
public record TopicCloseReceipt(boolean completed, Long completeItems, C2Exception error) {
    private static TopicCloseReceipt fromNative(boolean completed, long items, C2Exception error) {
        return new TopicCloseReceipt(completed, items == -1 ? null : items, error);
    }
}
