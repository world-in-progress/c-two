"""Shim — canonical module is ``c_two.crm.transferable``.

All names are re-exported so that existing ``from c_two.rpc.transferable …``
imports keep working.
"""
from c_two.crm.transferable import *  # noqa: F401,F403
from c_two.crm.transferable import (  # noqa: F401  — explicit re-exports
    transferable,
    auto_transfer,
    TransferableMeta,
    create_default_transferable,
    Transferable,
    register_transferable,
    get_transferable,
    _create_pydantic_model_from_func_sig,
    _TRANSFERABLE_MAP,
    _TRANSFERABLE_INFOS,
)
