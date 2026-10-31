# C-Two

C-Two exposes stateful resources to other processes and machines: a resource implements a CRM contract, a Host serves it under a route name, and clients call it through a typed proxy, in the same process, over local IPC or through a relay. This file fixes the words for these things. Chinese text keeps `Runtime`, `Host`, `relay`, `hold`, `buddy` and `SHM` in English.

## Language

### Resources and contracts

**CRM 契约 / CRM contract**:
The interface class decorated with `@cc.crm(namespace=..., version=...)` that declares the remotely callable methods; its method bodies are `...`. CRM stands for Core Resource Model.
_Avoid_: interface, service, ICRM, an `I` prefix on contract names

**资源 / resource**:
The plain object that implements a CRM contract and holds state. It is registered under a route name and is not decorated.
_Avoid_: service, servant, component; implementation (for the registered object)

**客户端 / client**:
Any code that calls `cc.connect(...)` and uses the proxy it returns.
_Avoid_: component, consumer

**代理 / proxy**:
The typed object returned by `cc.connect(...)`; calling its methods calls the resource.
_Avoid_: stub, handle

**路由名 / route name**:
The name chosen by the application in `cc.register(..., name=...)` and `cc.connect(..., name=...)`. It is not the CRM namespace.
_Avoid_: service name, resource name, endpoint (for a route)

**路由 / route**:
A route name bound to one resource in one Host, together with the contract identity it serves.
_Avoid_: binding, endpoint

**CRM 命名空间 / CRM namespace**:
The `namespace` argument of `@cc.crm`, part of the contract identity.
_Avoid_: 命名空间 alone when the endpoint namespace could be meant

**契约身份 / contract identity**:
The CRM namespace, name and version together with the ABI hash and the signature hash. A call reaches a route only when the client's contract identity matches.
_Avoid_: tag (for the whole identity), contract name

**契约发布 / contract release**:
A validated, persistent contract identity derived from canonical `c-two.contract.v2` content (`ContractReleaseRef`). It never contains a route name.
_Avoid_: contract version (for the whole release)

**可移植载荷 / portable payload**:
A FastDB `Payload` declared with `@cc.transfer(input=..., output=...)`; the data form every SDK can read. Python-only values travel as pickle instead.
_Avoid_: transferable

### Runtime and hosts

**Runtime**:
The object that owns one native session: registrations, connections, configuration overrides and shutdown. The module-level `cc.*` functions use the default Runtime.
_Avoid_: 运行时 or a lowercase runtime for this object (both name C-Two as a system), session, context

**Host**:
The server side of a Runtime: it listens on an IPC endpoint and serves the registered resources.
_Avoid_: 宿主, server process

### Transport

**直接 IPC / direct IPC**:
A same-machine connection to an `ipc://` address that needs no relay.
_Avoid_: local relay, native mode

**relay**:
The standalone `c3 relay` process that resolves route names across machines and forwards calls over HTTP. SDKs never embed or start it.
_Avoid_: 中继, proxy, gateway, broker

**relay 锚点 / relay anchor**:
The relay a process registers its routes with and resolves names through (`C2_RELAY_ANCHOR_ADDRESS`). Calls may travel through a different relay.
_Avoid_: home relay, registry

**端点 / endpoint**:
The operating-system object behind an `ipc://` address: a Unix socket in an endpoint namespace, or a Windows named pipe.
_Avoid_: address (for the OS object), socket (on Windows)

**端点命名空间 / endpoint namespace**:
The owner-controlled directory that holds Unix endpoints and their coordination files, identified by a disk format version.
_Avoid_: 命名空间 alone when the CRM namespace could be meant, IPC root

**凭据 / credential**:
The record a listener leaves about its endpoint, used later to inspect or reap that endpoint. It proves nothing about liveness.
_Avoid_: token, certificate

**握手 / handshake**:
The exchange that opens an IPC connection: the fixed version preamble, identity checks and route metadata.
_Avoid_: hello (for the whole exchange)

**物理连接 / physical connection**:
One established OS stream between two processes. Several proxies may share it, and the loans made over it are settled on it.
_Avoid_: connection (for a proxy), channel

**连接核心 / connection core**:
The internal state machine of one physical connection, holding its write queue, loan book and end state (`ConnectionCore`). It does no I/O itself.
_Avoid_: 负责方, coordinator, owner (for this object)

**停用 / deactivation**:
A physical connection refusing new calls while it stays open to settle its loans.
_Avoid_: close, shutdown (for this state)

**对端结束 / peer end**:
Proof that the peer's side of a physical connection is gone: a read returns EOF, or ECONNRESET on a Unix local stream.
_Avoid_: disconnect; timeout (as proof)

**分片 / chunk**:
One piece of a payload sent in several parts; over IPC the receiver puts the pieces back together by reassembly.
_Avoid_: 分块, fragment, segment

**重组 / reassembly**:
Putting the chunks of one payload back together on the receiving side, within the reassembly budget.
_Avoid_: 组装, 拼装, assembly

### Memory and lifetime

**租约 / lease**:
The transport's claim on a buffer from the moment a call delivers it until it is released.
_Avoid_: 句柄 (for the claim), lock

**借出 / loan**:
An allocation a pool owner has published to the peer of a physical connection, named by a `loan_id`. It ends with a release record from the peer or with peer end.
_Avoid_: 归还 (for the release), share

**借出表 / loan book**:
A pool owner's per-connection record of the loans it has made and not yet seen released.
_Avoid_: pending table

**hold**:
Keeping a response's lease after the call returns, with `cc.hold(...)` and `HeldResult`, until it is released.
_Avoid_: 持有 (for the API; 保留 is the verb in Chinese text), pin, cache

**借用输入 / borrowed input**:
A server-side input that is valid only while the resource method runs, selected at registration with `input_lifetime`.
_Avoid_: zero-copy input, view

**载荷所有者 / payload owner**:
The FastDB `Payload` object that owns a payload's memory; releasing a hold or ending a borrowed call invalidates it and its checked views first.
_Avoid_: 所有者 alone, FastDB owner

**属主 / pool owner**:
The process whose allocator owns a shared-memory pool; in the single-owner release design (单一属主释放) only it changes its allocator.
_Avoid_: 所有者

**buddy 池 / buddy pool**:
The shared-memory segments carved by the buddy allocator for ordinary payloads.
_Avoid_: SHM pool (for the buddy part alone)

**独占段 / dedicated segment**:
A shared-memory mapping made for one payload that does not fit the buddy pool.
_Avoid_: 专用段, large segment

**文件溢出 / file spill**:
A file-backed buffer used when shared memory is unavailable or over budget.
_Avoid_: disk cache, swap

**预算 / budget**:
A finite allowance of bytes or time that one scope may consume, such as the SHM backing bytes of one direction or the wait of one call.
_Avoid_: quota; limit (for a budget)

**释放 / release**:
Returning a lease or an allocation to its owner; a failed release stays with its holder.
_Avoid_: free (for a lease), close

**截止时间 / deadline**:
The absolute end of one bounded operation, such as a connect, a call, a Host start or a shutdown; every wait inside the operation shares it.
_Avoid_: 期限

**超时 / timeout**:
A configured or caller-passed duration after which something expires, such as `timeout=`, `startup_timeout` or `C2_IPC_HEARTBEAT_TIMEOUT`; a bounded operation computes its deadline from it.
_Avoid_: 期限
