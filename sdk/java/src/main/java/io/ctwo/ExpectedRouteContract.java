package io.ctwo;

import java.util.Objects;

/** Route name and the full CRM contract identity checked by Core when connecting. */
public record ExpectedRouteContract(
        String routeName, String crmNamespace, String crmName, String crmVersion,
        String abiHash, String signatureHash) {

    public ExpectedRouteContract {
        Objects.requireNonNull(routeName);
        Objects.requireNonNull(crmNamespace);
        Objects.requireNonNull(crmName);
        Objects.requireNonNull(crmVersion);
        Objects.requireNonNull(abiHash);
        Objects.requireNonNull(signatureHash);
    }
}
