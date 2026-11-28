package io.ctwo;

import java.util.LinkedHashMap;
import java.util.Map;

/** Immutable metadata transmitted with a call. Core validates names and limits. */
public record CallMetadata(Map<String, String> entries) {
    public CallMetadata {
        entries = Map.copyOf(entries);
    }

    public static CallMetadata empty() { return new CallMetadata(Map.of()); }
    public static CallMetadata of(Map<String, String> entries) { return new CallMetadata(entries); }
    public String get(String key) { return entries.get(key); }

    public CallMetadata withEntry(String key, String value) {
        Map<String, String> updated = new LinkedHashMap<>(entries);
        updated.put(key, value);
        return of(updated);
    }

    String[] nativeEntries() {
        String[] result = new String[entries.size() * 2];
        int index = 0;
        for (var entry : entries.entrySet()) {
            result[index++] = entry.getKey();
            result[index++] = entry.getValue();
        }
        return result;
    }

    static CallMetadata fromNative(String[] entries) {
        Map<String, String> fields = new LinkedHashMap<>();
        for (int index = 0; index < entries.length; index += 2) {
            fields.put(entries[index], entries[index + 1]);
        }
        return of(fields);
    }
}
