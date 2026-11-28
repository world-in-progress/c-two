package io.ctwo;

import io.ctwo.error.C2Exception;

/** Terminal observation or a receipt for an operation still closing. */
public record StreamCloseReceipt(boolean completed, Long completeItems, C2Exception error) {
    private static StreamCloseReceipt fromNative(boolean completed, long items, C2Exception error) {
        return new StreamCloseReceipt(completed, items == -1 ? null : items, error);
    }
}
