# 跨进程释放与持锁者失效：业界做法调研

日期：2026-10-31。起因：[过度防御复审](../design/2026-10-31-over-defense-review.md)第四节的 R07 失败实验。状态：调研记录与评估问题，没有方案，也没有改动代码。

## 问题

多个进程共用一个分配器或一把锁时，有两种失效：持锁者还活着、只是被挂起，业界称 lock-holder preemption；持锁者已经死了，常说 owner died。

C-Two 现在的做法：分配只由段的属主进程完成，对端直接在属主的段里释放，经段头里的共享分配器锁；锁字记录持锁者的 PID，用 PID 判断持锁者是否已死。R07 实验测得，持锁者被挂起时，释放自旋约 1.5 秒后失败；持锁者恢复后，显式重试 10 µs 内成功；走最终 Drop 放弃时，那块内存占在属主的池里直到属主退出。常规负载与 CPU 超订下百万次释放都没有失败，只有持锁那一刻被挂起超过 1.5 秒才会触发。

## 业界的三类做法

**单一属主：不跨进程共用锁，由一个进程负责分配与释放。** Ray 的共享内存段由对象存储进程创建，并用 dlmalloc 在映射文件上分配，worker 进程只把它映射进来；对象删除时由 Raylet 取消固定并释放共享内存（[Ray 论坛](https://discuss.ray.io/t/learning-about-shared-memory/9668)、[Object Spilling](https://docs.ray.io/en/latest/ray-core/internals/object-spilling.html)）。iceoryx 一代用跨进程互斥锁，踩过 owner died：应用持锁时被杀，守护进程 RouDi 回收资源时卡在这把锁上（[issue #2193](https://github.com/eclipse-iceoryx/iceoryx/issues/2193)），开发者会议讨论过的权宜办法是锁仍被占着就跳过清理、宁可漏掉（[2024-06-25 会议](https://github.com/eclipse-iceoryx/iceoryx/wiki/2024-06-25-Eclipse-iceoryx-developer-meetup)）。后继的 iceoryx2 在数据传输上不用锁，维护者在社区讨论中给出的理由是锁会造成死锁，尤其当持锁的应用崩溃时；它改为跟踪在途数据，回收崩溃应用的资源（[维护者的公开发言](https://lemmy.4d2.org/u/elBoberido@programming.dev)，属二手来源，未查到正式设计文档）。

**让数据结构容忍被抢占。** DPDK 用户在超订的虚拟机和容器上跑 rte_ring 时，持锁者被抢占的问题很普遍，20.05 版因此加入 HTS（头尾同步，完全串行）与 RTS（尾部只由最后一个完成者推进）两种同步模式；提案中的一组超订测试里，经典模式每个对象约 28 万个周期，新模式约 300 个（[RFC](https://mails.dpdk.org/archives/dev/2020-March/160786.html)、[v7](https://mails.dpdk.org/archives/dev/2020-April/164509.html)）。

**保留共享锁：持锁者活着就等，卡死或死掉才当致命错误。** PostgreSQL 的自旋锁只保护短小、无分支的代码，更长的等待用会睡眠的锁；自旋超过上限就 PANIC，重启全部后端。维护者也指出，这个报错常常不是锁真的被占着，而是机器没分到 CPU（[pgsql-hackers 讨论](https://postgresql.org/message-id/CA%2BTgmoaicDjVUnfOjdDXoD9Q%2Bcfzz4NSAk7GgTh-3STuoAA0vA%40mail.gmail.com)、[s_lock_stuck 讨论串](https://www.postgrespro.com/list/thread-id/2677778)）。POSIX robust mutex 在持锁者死亡时通知下一个加锁者（EOWNERDEAD），Linux 支持；macOS 27 的 SDK 头文件 `pthread.h` 里没有任何 robust 相关声明（Host 本机核对）。C-Two 的 PID 锁字起的是同一作用。

## 对 C-Two 的映射

C-Two 现在属于第三类，但多了一处少见的做法：用有界自旋把"持锁者活着但被挂起"也变成错误，再为这个错误配了"失败后保留以便重试"的整套状态与公开语义（R07）。

短期做法（复审中 R07 的改进版第三种）回到第三类的通行做法：释放先在原地尝试一次；遇到争用，交给后台的单一清理所有者，持锁者活着就一直等到释放成功，已死才放弃并计数。调用方不等、不报错，也不漏内存。

长期方向是第一类：对端不再直接释放，而是在已有连接上给属主发释放消息，由属主自己释放；对端崩溃或断开时，属主回收这个连接名下的块。这样每个分配器都只有属主一个进程在写，"其他进程持锁时被挂起或死去"这类失效从根上不存在。可以连带去掉的有：段头的跨进程锁字与 PID 检测、死亡观察、毒化标记、R07 的保留与重试、dedicated 段的 read_done 与崩溃超时，以及 IPC 结算层里为了不在分配器锁上阻塞而做的部分。

## 评估要先回答的问题

这些问题的回答与方案草图见[单一属主释放方案评估](../design/2026-10-31-single-owner-release-evaluation.md)。

1. 消息开销：每次 SHM 响应多一条释放消息，能否捎带到下一个请求帧上批量发送；需要用现有基准量化小调用与大载荷两种情形。
2. 请求块由谁释放：客户端收到响应后自己释放，还是服务端拷贝完后发"已消费"；前者在长时间运行的调用中会让大请求块一直占着。
3. 断线时的 hold：若属主在连接断开时回收该连接名下的块，held 视图必须先在客户端失效，即 hold 不再跨连接存活，这是公开语义的变化；若不回收，就需要按对端进程是否存活来判断，又回到跨进程的存活检测。
4. 协议与 ABI：新的消息类型与握手版本，Rust、Python、Node 一起改；宜与复审第五节的协议批合并，按 AGENTS.md 用确切候选跑全平台门禁。
5. 顺序：一旦采用第一类，R07 改进版、毒化标记（A7）、持锁者已死时的重复自旋（A 区待议项）以及 IPC 结算层的部分改造都会失去意义，因此应先完成这项评估，再实施那几项。
