"""Registration keeps the relay error alongside its original rollback receipt."""

from __future__ import annotations

from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import threading
import uuid

import pytest

import c_two as cc
from c_two.error import ContractMismatch, ResourceAlreadyRegistered, ResourceUnavailable


@cc.crm(namespace="test.registration-receipt", version="1.0.0")
class RegistrationReceipt:
    def ping(self) -> str: ...


class ReceiptResource:
    def ping(self) -> str:
        raise AssertionError("a refused registration must not enter the resource")


@pytest.mark.parametrize("code,name,status,error_type", [
    (703, "ResourceAlreadyRegistered", 409, ResourceAlreadyRegistered),
    (709, "ContractMismatch", 409, ContractMismatch),
    (702, "ResourceUnavailable", 503, ResourceUnavailable),
])
def test_registration_canonical_error_preserves_type_details_and_receipt(
    monkeypatch, code, name, status, error_type,
):
    # Error contracts: "including code, name, message, and structured details."
    route = f"receipt-{uuid.uuid4().hex}"
    details = {
        "name": route,
        "registrations": json.dumps([{
            "relay_id": "relay-a", "relay_url": "http://relay-a:8080",
            "registered_at": 1000.0, "relay_status": "Alive",
        }]),
        "extra": "all details survive\nincluding line breaks",
    }
    message = "canonical registration refusal"
    body = json.dumps({"version": 1, "code": code, "name": name,
                       "message": message, "details": details}).encode()
    requests = []

    class Relay(BaseHTTPRequestHandler):
        def do_POST(self):
            requests.append((self.path, json.loads(self.rfile.read(int(self.headers["Content-Length"])))))
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *args):
            pass

    monkeypatch.setenv("C2_ENV_FILE", "")
    root = Path("target/route-identity-b6-validation") / f"receipt-{uuid.uuid4().hex}"
    root.mkdir(parents=True, mode=0o700)
    relay = ThreadingHTTPServer(("127.0.0.1", 0), Relay)
    worker = threading.Thread(target=relay.serve_forever)
    worker.start()
    runtime = cc.Runtime()
    try:
        if os.name != "nt":
            runtime.set_local_endpoint(root=str(root.resolve()))
        runtime.set_relay_anchor(f"http://127.0.0.1:{relay.server_port}")
        with pytest.raises(error_type) as refused:
            runtime.register(RegistrationReceipt, ReceiptResource(), name=route)
        error = refused.value
        assert int(error.code) == code
        assert error.details == details
        if code == 703:
            assert error.message == f"Route name already registered with relay: {route!r}"
        else:
            assert error.message == f"registration failed at relay_prepare: {body.decode()}"
        native = error.__cause__
        assert native.category == "lifecycle"
        assert native.status_code == status
        assert native.code == code
        assert native.name == name
        assert native.details == details
        receipt = native.registration_failure
        assert receipt["failure_source"] == "relay_prepare"
        assert receipt["error_message"] == body.decode()
        assert receipt["status_code"] == status
        assert native.message == f"registration failed at relay_prepare: {body.decode()}"
        assert receipt["rollback"]["local_removed"] is False
        assert receipt["rollback"]["active_drained"] is True
        assert receipt["rollback"]["closed_reason"] == "registration_rollback"
        assert requests and all(path == "/_register" and request["prepare_only"] for path, request in requests)
    finally:
        try:
            cleanup = runtime.shutdown(timeout=10)
        finally:
            relay.shutdown()
            relay.server_close()
            worker.join(timeout=5)
        assert not worker.is_alive()
        assert cleanup["completed"]
