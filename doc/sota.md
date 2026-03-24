# SOTA Partterns for C-Two RPC

## 简述
在GIS领域，尤其是在科学计算、基于物理的模拟等高性能计算场景中，紧耦合模式为跨领域模型协同解决地理问题提供了重要的技术路径。传统或者狭义的紧耦合模式下，各个领域模型直接以单宿主机的形式运行，模型间通过共享内存(OpenMP、MPI-3 RMA等)或者高速网络(Infiniband RDMA等)进行通信。这种模式的优势在于通信效率高、延迟低，适合需要频繁交换大量数据的场景，如数值模拟中的网格数据交换、物理场景中的状态同步等。
然而，随着模型复杂度的增加和计算资源的多样化，传统的紧耦合模式面临着可扩展性和灵活性方面的挑战。为了克服这些限制，C-Two RPC（Coupled Two-way Remote Procedure Call）模式应运而生。C-Two RPC通过引入远程过程调用机制，使得不同领域模型可以在分布式环境中运行，并通过网络进行高效通信。这种模式不仅保留了紧耦合的高性能优势，还提供了更好的可扩展性和灵活性，适用于大规模分布式计算环境中的跨领域协同问题。但是，当前的C-Two设计以协议作为硬性区分，每一种协议的CRM Server只能对接一种协议的CRM Client，这在实际应用中可能会限制系统的灵活性和适应性。因此，真正的SOTA C-Two RPC模型应该能隐式支持多协议通信，将协议的选择和适配过程真正透明化，进一步提升不同领域模型之间的协同效率和系统的整体能力。

## SOTA C-Two RPC模型设计
一个真正的SOTA C-Two RPC模型应该具备以下几个关键特征：
- CRM作为资源的抽象，其服务器的启动与否应该作为一种隐式行为，而非显示启用。这样CRM所在的运行时环境就可以在提供资源服务的同时能直接进行计算业务，由此避免资源服务和计算业务之间的显示区分。一个典型的场景是，在耦合水动力模型中，潜水方程模块中的网格资源本身就参与了求解器的计算过程，如果沿用当前的C-Two设计，就需要有一个独立的资源进程来提供网格资源服务，这就导致直接依赖网格资源进行计算的求解器和网格资源之间必须通过进程间通信实现数据交换，极大降低了效率。如果还存在其他可耦合资源，如物理参数库、数据输入输出服务等，就需要更多的资源进程来提供相应的服务，这样就会导致系统中存在大量的资源进程，而这些资源进程本身并不直接参与计算业务，反而增加了系统的复杂性和通信开销。如果CRM本身在其所在进程中既能直接作为资源对象，可以直接参与计算业务，又能隐式提供资源服务，那么就可以大大简化系统架构，减少通信开销，提高系统的整体效率。
- CRM本身一定是一个普通的资源对象，C-Two Server比起服务，更像是一种资源托管器。当一个CRM被注册给C-Two后，其可继续完成自己所在模型的计算业务，而当计算业务结束后，CRM并不会被GC，而是因为C-Two的托管而继续在该进程中存在，直到该进程结束。这就意味着，模型所在的计算节点在模型计算过程中，扮演的是全能角色(计算节点+资源托管器)，而当计算业务结束后，模型所在的计算节点就退化为一个单纯的资源托管器，这样就实现了计算业务和资源服务之间的无缝切换，如此可以保证最优资源局部性，即在计算过程中资源始终在计算节点本地，避免了资源服务和计算业务之间的通信开销，而当退化为资源节点时，资源服务也直接源于计算节点本地，避免了资源回传到特定资源节点再进行资源服务的通信开销。
- C-Two RPC方案是注册+获取式的。即资源对象注册到C-Two后，其他线程的，进程的，甚至其他计算节点的模型都可以通过get方式获取到该资源对象的引用，并直接调用其方法来进行资源访问和操作。这样就实现了资源对象的全局可见性和可访问性。同时，C-Two RPC方案还应该支持资源对象的动态注册和注销，以适应模型计算过程中资源需求的变化。

### 模式设计
基于上述设计原则，SOTA C-Two RPC模型的设计可做出如下调整：

- C-Two 进程级注册器
```python
cc.register(namespace_as_address, icrm, crm_data, shutdown_callback)    # 可能还有其他参数
```
    通过上述接口，模型中的资源对象可以直接注册到C-Two中，注册过程会隐式在一个线程中启动IPC-v2服务。任何一个线程中的CRM对象都可以通过上述接口注册到C-Two中，注册完成后该CRM对象就成为了一个全局可见的资源对象，可以被其他线程、进程甚至计算节点上的模型直接访问和操作。

- C-Two 资源访问接口
```python
# 获取资源对象的引用
crm_ref: ICRM_CLASS = cc.connect(namespace_as_address)    # 可能还有其他参数
# 关闭资源对象的引用
cc.close(crm_ref)    # 可能还有其他参数
```
    通过上述接口，模型中的计算业务可以直接获取到C-Two中注册的资源对象的引用，并直接调用其方法来进行资源访问和操作。这样就实现了资源对象的全局可见性和可访问性，同时也避免了资源服务和计算业务之间的通信开销，提高了系统的整体效率。

- C-Two 资源注销接口
```python
cc.unregister(namespace_as_address)    # 可能还有其他参数
```
    通过上述接口，模型中的资源对象可以从C-Two中注销，注销过程会隐式关闭IPC-v2服务。任何一个线程中的CRM对象都可以通过上述接口从C-Two中注销，注销完成后该CRM对象就不再是一个全局可见的资源对象，其他线程、进程甚至计算节点上的模型将无法访问和操作该资源对象。

- C-Two 线程优惠
```python
crm_instance: CRM_CLASS = cc.connect(icrm, namespace_as_address)    # 可能还有其他参数
```
    如果get接口中的namespace_as_address中的协议是线程内协议，那么直接返回CRM对象实例，完全规避RPC调用的通信开销(但仍需遵守读写并发规则)，确保最大化的资源局部性和访问效率。亦即在耦合水动力场景下，潜水方程模块中的网格资源在注册到C-Two后，水动力求解器模块中的计算业务在获取该网格资源时，如果协议是线程内协议，那么直接返回该网格资源的实例，水动力求解器模块中的计算业务就可以直接访问和操作该网格资源，而无需通过RPC调用来进行通信，从而大大提高了系统的整体效率。

- C-Two 跨节点Http协议通信
    SOTA C-Two本身没有直接的Http级别CRM服务模式(但有Client，用来支持跨节点访问)，但可以通过利用FastAPI等框架建立一个异步routing机制实现捕获Http请求后立刻通过IPC-v2协议将请求转发给CRM对象来处理的模式，从而实现跨节点的Http协议通信。亦即，CRM Ref (Http client) -> FastAPI (Http server) router -> Routing (IPC-v2) -> CRM Object (CRM Server) -> Routing (IPC-v2) -> FastAPI (Http server) response -> CRM Ref (Http client) response。通过上述模式，SOTA C-Two RPC模型就可以支持跨节点的Http协议通信，从而进一步提升了系统的灵活性和适应性，满足了更多样化的应用需求。Routing机制可以参考memory_routing.py，但性能需要更优化，以减少该模式下多次转发带来的通信开销。

## 设计评审

基于对C-Two当前代码库（v0.2.7）的完整审读，以下从设计合理性、技术可行性和与主流RPC框架的对比三个维度，对上述SOTA方案进行评估。

### 一、设计合理性

#### 1.1 "CRM即资源对象，Server即托管器"

核心观察——当前设计中CRM Server是一个独立的服务进程，而CRM对象本身应该既参与本地计算又能被远程访问——准确指向了一个真实的架构痛点。

当前架构中，`ServerConfig`要求显式传入`crm`实例和`bind_address`，然后`Server.start()`启动事件循环来服务请求。这意味着CRM对象被"锁"在Server的事件循环中。计算节点如果既要本地使用网格数据又要对外提供服务，必须走`thread://`协议绕一圈。在耦合水动力场景中这是不必要的开销。

`cc.register()`隐式托管模型的本质是**Service Mesh中Sidecar模式向进程内下沉**，在HPC领域是有价值的方向。

**需要注意的问题：**

- **线程安全性的隐式复杂度**：当前`_Scheduler` + `_WriterPriorityReadWriteLock`已在处理并发问题。若CRM对象既被本地计算线程直接访问，又被IPC-v2服务线程通过RPC访问，并发控制的边界变得模糊。设计中提到"仍需遵守读写并发规则"，但这意味着本地调用者也必须经过某种锁或调度器，否则会出现数据竞争。这在实践中容易被忽视。
- **对象生命周期管理**：设计中提到"计算业务结束后CRM不会被GC，因为C-Two的托管"。这引入了隐式的引用计数/生命周期管理问题。Python的GC依赖引用计数+循环检测，若C-Two持有强引用，即使本地计算逻辑已释放对CRM的引用，CRM也不会被回收。`unregister()`的语义虽然已设计，但在异常退出、进程崩溃等场景下的清理机制需要额外考虑。

#### 1.2 "注册+获取式"与服务发现模式

`cc.register(namespace, icrm, crm_data)` + `cc.connect(namespace)`本质上是一个**进程内/进程间的服务注册表（Service Registry）**。

**合理之处：**
- 统一的namespace寻址消除了协议感知的复杂性
- 动态注册/注销适应了GIS模拟中资源按需创建的场景
- 与当前URI scheme选择方式（`thread://`、`ipc-v2://`等）相比，对调用者更友好

**潜在问题：**
- **命名冲突和命名空间治理**：在分布式多节点场景中，`namespace_as_address`的唯一性如何保证？当前`__tag__`格式为`{namespace}/{class_name}/{version}`，但若两个节点上的不同模型注册了同名资源，需要某种分布式一致性或至少一个中心化的名称服务来解决冲突。
- **版本兼容性**：当前ICRM已有`version`字段（语义版本），但SOTA设计中`cc.connect()`没有显式传入版本要求。在耦合场景中可能导致版本不匹配的隐蔽bug。

#### 1.3 "线程优惠"——最有价值的设计点

当`cc.connect()`检测到目标资源在同一进程内时直接返回CRM实例，这是整个设计中**最务实、最有价值的优化**。

当前的`thread://`协议虽然有`call_direct()`快速路径绕过序列化，但调用者仍需知道自己要用`thread://`协议。SOTA设计将这个决策自动化了。

**深入思考：**
- 直接返回CRM实例意味着调用者拿到的是**真实对象引用**而非代理。这与远程场景下拿到的代理对象在语义上不同（例如本地调用失败抛出Python异常，远程调用失败抛出`CCError`）。这种**语义不一致**可能在调试时造成困惑。
- 一个可能更优的设计：返回一个**轻量级本地代理**，底层直接委托给CRM实例，但保持与远程代理相同的异常语义和并发控制。类似Java RMI的本地stub优化。

### 二、技术可行性

#### 2.1 隐式IPC-v2服务启动

**技术上完全可行。** 当前的`IPCv2Server`使用asyncio事件循环，可以在独立线程中运行。需要注意：

- IPC-v2 Server绑定Unix Domain Socket，路径需要确定性生成（基于namespace hash），否则客户端无法寻址
- 当前`ipc_server.py`的`start()`方法已是异步的，包装在daemon线程中启动是标准模式
- **资源开销**：每个注册的CRM都启动一个asyncio线程 + UDS listener。若一个进程中注册了数十个CRM，线程数量和文件描述符消耗需要关注。更优的方案可能是**单个IPC-v2 Server多路复用多个CRM**（类似gRPC的单端口多服务），通过在wire层面的`CRM_CALL`消息中加入namespace字段实现路由

#### 2.2 跨节点HTTP路由

设计链路：`CRM Ref (Http client) → FastAPI → IPC-v2 → CRM Object → IPC-v2 → FastAPI → CRM Ref`

**可行，但有性能顾虑：**
- 这本质上是**Reverse Proxy + Protocol Translation**模式，类似Envoy做gRPC-Web到gRPC的转换
- 问题在于**双重序列化/反序列化**：HTTP层一次，IPC-v2层一次。对于大型网格数据（GB级），会显著增加延迟
- **更优的替代方案**：FastAPI接收到HTTP请求后，直接将原始wire bytes通过IPC-v2的`relay()`转发，而不是解码再编码。当前`router.py`中的`Client.relay()`已实现了这一模式，可以直接复用

#### 2.3 进程级注册器的全局状态管理

`cc.register()`作为进程级单例，需要管理注册表（namespace → CRM + metadata）、IPC-v2服务线程池和跨进程发现机制。

**可行性：** 类似`multiprocessing.Manager`或`ray.put()`/`ray.get()`的模式。但需要注意Python的`fork()`语义——若计算模型使用`multiprocessing`创建子进程，注册器状态不会自动继承到子进程中。

### 三、与主流RPC框架对比

#### 3.1 vs gRPC

| 维度 | gRPC | SOTA C-Two |
|------|------|-----------|
| 接口定义 | Protobuf IDL (.proto文件) | Python装饰器 (@icrm) |
| 序列化 | Protobuf (固定schema) | Transferable (可扩展, 支持Arrow) |
| 传输 | HTTP/2 | IPC-v2 (UDS+SHM) / HTTP / ZMQ |
| 本地优化 | 无 (始终走HTTP/2) | 线程优惠 + 零拷贝SHM |
| 资源模型 | 无状态Service | 有状态CRM |
| 并发控制 | 无 (由用户处理) | 内建Reader-Writer Lock |

**C-Two的优势：**
- gRPC没有本地快速路径，即使server和client在同一进程中也要走HTTP/2。C-Two的线程优惠是真实的差异化优势。
- gRPC的Protobuf序列化对科学计算中的大型数组/矩阵不友好（没有零拷贝支持），C-Two的SHM数据平面在本地IPC场景下性能优势明显。
- gRPC面向无状态RPC，C-Two面向有状态资源，后者更适合GIS中的长生命周期计算对象。

**C-Two的劣势：**
- gRPC有成熟的负载均衡、健康检查、截止时间传播、拦截器链等生产级特性。C-Two的`router.py`目前只是基础的命名路由。
- gRPC的跨语言支持（C++/Java/Go/Rust等）对HPC生态很重要，很多核心计算库不是Python。C-Two目前是纯Python实现。

#### 3.2 vs Ray

| 维度 | Ray | SOTA C-Two |
|------|-----|-----------|
| 资源模型 | Actor (有状态) | CRM (有状态) |
| 调度 | 集群级分布式调度 | 无调度，手动寻址 |
| 数据传输 | Plasma Object Store (SHM) | IPC-v2 SHM Pool |
| 本地优化 | 同进程Actor直接调用 | 线程优惠 |
| 容错 | Actor自动重启 | 无 |

这是最接近的对比。Ray的Actor模型和C-Two的CRM模型在哲学上非常相似。关键区别：
- Ray是通用分布式计算框架，C-Two专注于耦合资源模型。C-Two更"领域特定"，既是优势（更贴合GIS场景）也是劣势（生态小）
- Ray的Plasma Object Store与C-Two的SHM Pool思路相似，但Ray的实现更成熟（引用计数、溢出到磁盘、自动GC）
- **关键差异**：Ray的`ray.get()`是按值传递（反序列化副本），SOTA设计中`cc.connect()`的线程优惠是按引用传递。在GIS场景中，按引用传递对GB级网格数据是巨大的性能优势

#### 3.3 vs Cap'n Proto RPC

Cap'n Proto的Promise Pipelining允许在同一次调用中链式操作远程对象，减少网络往返。SOTA设计未涉及这一点。对于耦合模型中"获取网格 → 查询属性 → 计算"的链式调用模式，引入pipelining或batching可能是一个值得考虑的未来方向。

#### 3.4 vs MPI（HPC视角）

MPI的核心优势是集合通信（MPI_Allreduce、MPI_Scatter等），在PDE求解器的网格分区场景中不可替代。C-Two的RPC模型更适合**非对称访问模式**（一个模型查询另一个模型的资源），而非**对称通信模式**（所有rank参与归约）。SOTA C-Two不应试图替代MPI，而应定位为**MPI之上或之外的协同层**——处理不同模型之间的资源共享，而非单个模型内部的并行通信。

### 四、关键设计建议

#### 4.1 单端口多路复用

当前设计暗示每个`cc.register()`调用启动一个独立的IPC-v2服务线程。建议改为进程级单例IPC-v2 Server，多路复用所有注册的CRM：

```python
cc.register('ns1', IGrid, grid)      # 注册到同一个Server
cc.register('ns2', ISolver, solver)   # 共用同一个UDS + 事件循环
```

减少线程数量和文件描述符消耗，路由基于namespace在wire层面完成（在`CRM_CALL`消息中加入namespace字段）。

#### 4.2 统一代理对象

建议`cc.connect()`始终返回代理对象（Proxy），而非在线程内返回原始实例、跨进程返回ICRM stub：

```python
crm_ref = cc.connect('ns1')  # 始终返回Proxy
# Proxy内部：
#   - 线程内：直接委托 + 共享调度器的并发控制
#   - 进程间：IPC-v2 RPC
#   - 跨节点：HTTP → IPC-v2
```

统一语义避免调用者需要关心底层传输，同时保持异常语义和并发控制的一致性。

#### 4.3 异步接口支持

当前设计全部是同步调用。在耦合模拟中，若模型A需要模型B的网格数据来推进时间步，但模型B的当前时间步尚未完成，同步调用会阻塞。建议支持异步获取模式：

```python
future = cc.connect_async('ns1')
# 或
async with cc.connect('ns1') as crm:
    result = await crm.get_grid_infos(level, ids)
```

#### 4.4 shutdown_callback语义明确化

`cc.register(namespace, icrm, crm_data, shutdown_callback)`中的`shutdown_callback`需要明确：在`unregister()`时调用？进程退出时调用？远程`shutdown`信号到达时调用？这些场景的语义应予以区分。

### 五、总体评价

**设计方向是正确的。** 核心问题的识别——CRM不应被Server绑架，而应作为可托管的资源对象存在——准确指向了当前C-Two在耦合计算场景中的架构瓶颈。

**最具价值的三个设计点：**
1. 隐式服务托管（register/unregister）——简化了模型开发者的心智负担
2. 线程优惠（同进程直接返回实例）——对GIS耦合场景有切实的性能价值
3. 计算节点的角色退化（全能 → 资源托管器）——符合HPC中计算+数据共存的实践

**需要进一步完善的三个方面：**
1. 并发控制在隐式托管模式下的清晰边界（本地调用 vs RPC调用需共享同一个调度器）
2. 多CRM的服务多路复用，避免每个CRM一个线程+一个UDS
3. 跨节点HTTP路由中的双重编解码问题（应复用`relay()`式的passthrough）

**定位对标：** 该设计与Ray Actor模型最接近，但在GIS耦合场景中的资源局部性优化（线程优惠 + SHM零拷贝）是真正的差异化竞争力。若要走向生产级，跨语言支持和容错机制将是下一个需要攻克的方向。

## 附录：SHM Pool与Ray Plasma对比增强分析

针对设计评审中提出的"Ray的实现更成熟（引用计数、溢出到磁盘、自动GC）"这一不足，以下对三项增强的可行性进行逐一分析，并附评审意见。

### 核心定位差异

在讨论具体增强项之前，需要明确一个根本性的定位差异：

- **Ray Plasma** = **共享对象存储**——对象生命周期长、多消费者、跨节点分享
- **C-Two SHM Pool** = **传输加速通道**——数据生命周期等于单次RPC（毫秒级）、点对点、连接绑定

这个定位差异决定了不应盲目照搬Plasma的全部特性，而应根据C-Two自身的设计哲学有选择地增强。

### 当前架构对比

| 维度 | C-Two SHM Pool | Ray Plasma Object Store |
|------|---------------|------------------------|
| **定位** | 连接级IPC数据通道 | 集群级共享对象存储 |
| **内存模型** | 每连接1个固定256MB双向SHM | 节点级大内存池 (dlmalloc分配器) |
| **对象语义** | 无对象概念——纯buffer读写 | 不可变对象 (open → seal → immutable) |
| **引用计数** | 无——依赖同步RPC流控 | 分布式引用计数 (ObjectRef) |
| **磁盘溢出** | 无 | LRU驱逐 → 本地磁盘/S3 |
| **自动GC** | 仅时间戳扫描 (30s/120s) | 引用归零即回收 |
| **零拷贝** | memoryview直接读pool SHM | mmap fd传递 |
| **并发模型** | 同步RPC (请求/响应不并存) | 异步多读者 + seal后不可变 |
| **生命周期** | client创建+拥有, server track=False | Plasma Store进程统一管理 |

### A.1 引用计数

**Ray做法：** 每个`ObjectRef`是handle，跨进程/节点传播时递增计数，所有handle离开作用域后通知store回收。

**C-Two现状：** 无引用计数。同步RPC模式保证了安全性——请求写入SHM → client阻塞等待 → server读取处理 → 写回响应 → client读取。pool SHM上同一时刻只有一方在操作，不需要引用计数。

**结论：当前不需要，但未来Router/Worker架构下可能需要。**

- 当前同步RPC一问一答模式下pool buffer天然互斥，引用计数是多余的
- 若引入pipeline/streaming RPC或Router转发（一份数据被多个Worker读取），则需要引用计数确保buffer区域不被提前覆写
- 实现路径：不需要Plasma那样的分布式引用计数。轻量方案为pool buffer内部按slot分配，每个slot维护`refcount: int`，写入方`acquire_slot() → refcount=1`，转发时`ref_inc()`，读完`ref_dec()`，归零时slot标记可回收
- 复杂度中等，需要改造pool从"单一整块buffer"到"slot化分配器"

**评审补充：**

1. **slot化分配器的false sharing问题**：若多个slot的refcount落在同一个cache line（64 bytes）上，跨核访问会造成cache line bouncing。建议每个slot header按64字节对齐。
2. **Python 3.13+ free-threaded build下的原子性**：`threading.Lock`在free-threaded build中仍然是正确的同步原语，用锁是安全的。真正的风险是有人忘了加锁做裸`refcount += 1`。若追求极致性能，`ctypes`操作SHM中的原子字段（利用`mmap`的内存一致性语义）是更好的路径。

### A.2 磁盘溢出

**Ray做法：** 内存不足时Raylet按LRU将对象序列化写入磁盘/S3，访问时透明恢复。

**C-Two现状：** 超过pool大小(256MB)时回退到per-request SHM（每次create/destroy系统调用），超过16GB则失败，无磁盘回退路径。

**结论：不建议在框架层实现。**

理由：
1. **场景不匹配**：C-Two是连接级同步RPC通道，数据生命周期是毫秒级，不是Ray那种分钟/小时级。磁盘溢出的延迟（毫秒级SSD / 十毫秒级HDD）会直接打爆IPC-v2的微秒级延迟优势。
2. **已有退化路径**：pool放不下时已有per-request SHM回退，虽然慢但功能正确。大数据应由CRM自身管理（如GIS场景的tile数据可mmap磁盘文件）。
3. **复杂度极高**：需要LRU驱逐策略、异步I/O管线、序列化/反序列化、恢复路径、错误处理，远超IPC传输层的职责。
4. **替代方案更优**：
   - **动态pool扩容**：按需创建多个SHM segment（比磁盘溢出简单100倍）
   - **分块传输**：大payload拆成chunk流式传输
   - **CRM层面控制**：让CRM方法返回迭代器/流，避免一次性传输超大对象

**评审补充：**

此项分析是整份文档中最精彩的部分。补充一点：单次RPC传输16GB本身就是一个设计smell。若某个CRM方法需要返回如此大的数据，应在接口设计层面拆分（如分tile查询），而非在传输层兜底。这进一步验证了"不在框架层做磁盘溢出"的正确性。

此外，动态pool扩容（多segment）的优先级建议提升至P0。当前pool满时回退到per-request SHM的性能下降是显著的（每次`shm_open/ftruncate/mmap/munmap/shm_unlink`五个系统调用），而多segment的实现复杂度比引用计数和分代GC都低。具体可用**segment chain**模式：第一个segment满时分配第二个，元数据记录在header中。

### A.3 自动GC

**C-Two现状：**
- Pool SHM：client disconnect时unlink（正常路径）
- Per-request SHM：读完立即unlink（正常路径）
- 泄漏兜底：server端30s扫描一次，>120s的SHM segment强制清理
- 启动时清理：`cleanup_stale_shm()`清理上次崩溃残留

**结论：值得适度增强。**

当前GC的问题：
1. 120s延迟太长：高频per-request SHM场景下泄漏的segment会累积，极端情况下可耗尽`/dev/shm`
2. 无进程存活检测：client被SIGKILL时socket未关闭，pool SHM成为孤儿
3. 无内存压力感知：不管`/dev/shm`用了10%还是90%，GC行为完全相同

建议增强方案：

| 增强项 | 复杂度 | 收益 |
|--------|--------|------|
| **心跳检测**——client/server定期PING/PONG，超时视为断连触发清理 | 低 | 高——秒级发现崩溃 |
| **分代GC**——新创建的per-request SHM用短超时，pool SHM用长超时 | 低 | 中——减少临时SHM累积 |
| **内存压力感知**——读`/dev/shm`使用率，高压时缩短GC间隔并主动decay pool | 中 | 中——防止OOM |
| **owner PID检测**——检查SHM owner进程是否存活（`os.kill(pid, 0)`） | 低 | 高——精准识别孤儿SHM |

**评审补充：**

1. **分代GC的超时值需要自适应**：原方案建议per-request SHM用固定10s短超时，但若CRM方法本身执行时间较长（如一次完整的水动力模拟步进可能需要数十秒），10s超时会导致响应SHM在server写回之前被GC回收。建议改为：`timeout = max(30s, 2 × 最近N次RPC的P99延迟)`，即自适应而非固定值。
2. **内存压力感知的隔离性**：`/dev/shm`可能被其他进程（如PostgreSQL、Docker容器）共用，单纯看总使用率可能导致C-Two为别人的内存消耗买单。建议额外维护一个**C-Two自身创建的SHM segment列表**（可基于命名前缀`cc`/`ccpr_`过滤），只对自己管理的部分做压力判断。

### 综合优先级

```
P0 (值得立即做):
  ├─ 心跳检测 + 断连清理
  ├─ owner PID检测
  └─ Pool动态扩容 (多segment, 替代磁盘溢出) ← 从P1提升

P1 (下一阶段):
  ├─ 自适应分代GC策略
  └─ 内存压力感知 (隔离C-Two自身SHM)

P2 (Router/Worker架构引入后):
  └─ Slot化pool + 轻量引用计数 (注意cache line对齐)

不建议做:
  └─ 磁盘溢出 (留给CRM层/应用层)
```

## 附录：Rust Routing Server 方案

### 背景与动机

SOTA设计中的跨节点HTTP路由链路为：`CRM Ref (Http client) → FastAPI → IPC-v2 → CRM Object → IPC-v2 → FastAPI → CRM Ref`。当前Python实现（`router.py`）使用Starlette + uvicorn + ThreadPoolExecutor(16)，存在以下瓶颈：

- 每次relay都创建/销毁`Client`连接（`router.py:240-245`），无连接复用
- ThreadPoolExecutor线程数固定，高并发下成为瓶颈
- Python GIL导致即使是纯I/O转发也无法充分利用多核
- Starlette的ASGI层引入额外的Python对象分配开销

鉴于Routing Server的职责是**纯字节透传**——接收HTTP请求体的wire-format bytes，通过IPC-v2协议转发给CRM Server，等待响应后回传HTTP client——这恰好是Rust的最佳适用场景：纯I/O密集、无需理解业务语义、对延迟和吞吐量有极高要求。

### 为什么必须实现完整的SHM Pool

初始评估曾建议只实现inline路径（< 16MB），理由是"跨节点网络延迟会掩盖SHM开销"。但这一判断忽略了两个关键场景：

1. **同节点HTTP relay**：典型部署模式下，Rust Routing Server和CRM Server在同一台机器上。HTTP → IPC-v2这段是本地通信，SHM pool的零系统调用优势直接生效。若routing server只走inline + per-request SHM，相当于在关键路径上人为引入5个系统调用（`shm_open`/`ftruncate`/`mmap`/`munmap`/`shm_unlink`），而这本可避免。

2. **前端可视化直访大payload**：C-Two的目标之一是资源一致性访问。当接入fastdb并支持TypeScript的ICRM codegen + HTTP client后，前端（如WebGL/WebGPU可视化引擎）将直接通过HTTP访问CRM资源。GIS可视化中，一次tile请求返回的mesh/raster数据可能是几十MB甚至更大。链路为：

```
Browser (TS) → HTTP → Rust Routing Server → IPC-v2 (SHM pool) → CRM Server (Python)
```

此链路中Rust Routing Server是唯一可优化的性能瓶颈点（browser和CRM不可控），必须尽可能高效。SHM pool在这个场景下不是锦上添花，而是必要的。

### 需要实现的IPC-v2协议范围

Routing Server需要实现IPC-v2协议的**完整client子集**：

**必须实现：**
- Frame header编解码（16 bytes: `total_len` + `request_id` + `flags`）
- UDS连接管理（`tokio::net::UnixStream`）
- Pool SHM handshake（`FLAG_HANDSHAKE`）——创建pool segment、发送握手帧、接收ACK
- Pool SHM读写（`FLAG_POOL`）——请求写入pool、从pool读取响应
- Per-request SHM回退（`FLAG_SHM`）——pool满时的降级路径
- Inline frame收发——小payload的快速路径
- Socket path生成——必须与Python端`_resolve_socket_path(region_id)`完全一致

**不需要实现：**
- Wire format内部解析（`CRM_CALL`/`CRM_REPLY`的method name、error code等）——纯透传
- Transferable序列化/反序列化——由CRM Server和Client各自处理
- Scheduler/并发控制——routing server只是转发器

### Rust技术栈

```
HTTP server:    axum (基于hyper + tokio, 零拷贝body处理)
UDS client:     tokio::net::UnixStream (异步UDS)
SHM操作:        nix crate (shm_open/mmap/munmap/shm_unlink POSIX API)
连接池:         自建异步池 或 bb8/deadpool
frame编解码:    手写 (trivial, ~100行)
```

### 相比当前Python实现的改进

| 维度 | 当前Python (router.py) | Rust Routing Server |
|------|----------------------|---------------------|
| 并发模型 | ThreadPoolExecutor(16) + 同步socket | tokio async, 数千并发task |
| 连接管理 | 每次relay创建/销毁Client | 持久异步连接池 |
| SHM支持 | 完整（继承IPCv2Client） | 完整（Rust原生POSIX SHM） |
| 内存开销 | 每线程~8MB栈 + Python对象 | 每task~几KB |
| HTTP解析 | Starlette (Python ASGI) | hyper (零拷贝) |
| 吞吐量 | ~1K-5K req/s (估计) | ~50K-200K req/s (估计) |
| 尾延迟 | GIL + ThreadPool调度抖动 | 稳定，无GIL |
| 部署 | 纯Python，零编译 | 需预编译二进制 |

### 代码组织——按SDK标准设计

既然Rust端要实现完整的IPC-v2 client（含SHM pool），这实质上是**C-Two的第一个非Python SDK**。建议从一开始就按可复用SDK的标准组织代码：

```
c-two-rs/
├── crates/
│   ├── c2-wire/        # wire format编解码 (frame header, flags, 常量)
│   ├── c2-ipc/         # IPC-v2 client (UDS + SHM pool + handshake)
│   └── c2-router/      # HTTP routing server (axum + c2-ipc)
├── Cargo.toml          # workspace
└── ...
```

这样`c2-wire`和`c2-ipc`将来可以被Rust/C++计算模型直接依赖，实现跨语言CRM Client。Routing Server只是这个SDK的第一个消费者。这也是C-Two走向跨语言生态的**最低风险切入点**——从一个职责明确、边界清晰的组件开始，逐步扩展。

### 构建与分发——GitHub Action自动编译

引入Rust编译链后，需要确保Python用户的安装体验不受影响。建议方案：

**构建工具**：独立二进制（非PyO3扩展模块）。理由：
- Routing Server作为独立进程运行，不需要嵌入Python解释器
- 独立二进制方便Docker部署、systemd管理
- 避免PyO3引入的Python ABI兼容性问题
- Python侧通过`cc.router.start()`用`subprocess`启动，传入配置参数

**分发方式**：参考`ruff`的模式——平台特定wheel中附带可执行文件，Python端提供thin wrapper调用。

**GitHub Action配置**：

```yaml
# 触发条件：tag推送或pyproject.toml版本变更
# 构建矩阵：
strategy:
  matrix:
    os: [ubuntu-latest, macos-latest, windows-latest]
    target: [x86_64, aarch64]
    # aarch64使用cross-compilation (cross-rs或zig cc)

# 步骤：
# 1. cargo build --release (各平台)
# 2. 将二进制打包进platform-specific wheel
# 3. 发布到PyPI
# Python用户: pip install c-two 自动获取预编译routing server
```

主流Rust-Python混合项目（cryptography、pydantic-core、ruff）均采用此模式，工具链成熟。

### 风险与缓解

| 风险 | 缓解措施 |
|------|---------|
| IPC-v2协议兼容性——UDS socket path生成须与Python端完全一致 | 共享协议常量文件，集成测试覆盖Python server ↔ Rust client互操作 |
| SHM pool handshake实现偏差 | 编写Rust ↔ Python的端到端测试，验证pool创建、握手、读写、decay全流程 |
| 跨平台SHM差异——macOS的POSIX SHM有32字符名称限制 | 复用Python端已有的`shm_name()`哈希截断逻辑 |
| Windows不支持UDS+POSIX SHM | Windows上退化为TCP + named pipe，或不提供routing server（Windows非HPC主流部署平台） |
| 维护成本——两套语言的IPC-v2实现 | 协议版本号管理，CI中Python↔Rust互操作测试作为必过门禁 |

### 战略意义

Rust Routing Server不仅是一个性能优化，更是C-Two架构演进的关键节点：

1. **全栈资源访问层**：Browser (TS) → Rust Router → Python CRM，将C-Two的资源一致性访问从"Python内部"扩展到"全栈可达"
2. **跨语言SDK的起点**：`c2-wire` + `c2-ipc` crate是未来Rust/C++ CRM Client的基础
3. **性能关键路径的语言下沉**：把性能敏感的纯I/O组件下沉到系统语言，把业务逻辑留在Python的生产力优势中——这是现代Python生态（pydantic-core、ruff、polars）验证过的成功模式