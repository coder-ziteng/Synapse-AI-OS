# 设计文档 05：跨 OS 外交协议 (Inter-OS Diplomacy Protocol)

> 状态：DRAFT / **远期骨架**（Phase 6+，首期仅登记不实现）
> 关联需求：原始构想 "未来的远程访问，本质上就是两个 agent 之间的交互"、"外交工具"系列
> 关联里程碑：Phase 6+（远期）；与 [设计文档 04](04-diplomat-channel-architecture.md) 共享外交工具实现
> 最后更新：2026-09-25

---

## 0. 文档目的与定位

定义 Synapse 实例与其他 AI OS（无论是否基于 Synapse）交互时的**身份层、协议层、行为契约层**。原始构想明确判断："未来的远程访问本质就是两个 Agent 之间的交互"——这要求外交工具不仅是本机网络出口，还要承担**跨 OS 信任协商**。

> ⚠️ **本期定位**：本文档是**远期骨架**，仅固化**必须提前决策的协议兼容性约束**（如 DID 命名空间、信封格式），保证 Phase 5 的外交工具不会写出与未来跨 OS 协议不兼容的格式。具体协议细节待 Phase 6+ 收口。

---

## 1. 设计原则

| # | 原则 | 反模式 |
|---|------|--------|
| D1 | **身份去中心化**：每个外交工具有自有的去中心化身份（DID），不依赖中心化 CA | 复用 X.509 PKI 或 OAuth |
| D2 | **能力以凭证表达**：可声明的能力以可验证凭证（VC）形式签发 | 静态 token + 长期密钥 |
| D3 | **凭证短时化**：每次跨 OS 会话使用任务绑定凭证（短期 JWT / SD-JWT），任务结束即失效 | 长期 API Key |
| D4 | **行为可审计可追溯**：每次外交行为（请求内容 / 对方身份 / 执行结果）形成不可篡改审计链 | 仅本地日志 |
| D5 | **违约可惩罚**：违反行为契约 → 信誉扣减、权限回收、网络隔离 | 单次违规即放任 |
| D6 | **协议版本协商**：外交信封携带协议版本号，支持渐进式升级 | 全网强制同步升级 |

---

## 2. 身份层 (Identity)

### 2.1 去中心化身份 (DID)
- 每个 Synapse 实例在启动时由外交工具生成自己的 DID（`did:synapse:<hash>`），基于 W3C DID 规范。
- 私钥存于外交工具进程内存（**永不出外交工具**）；公钥嵌入 DID Document，可通过 gossip 协议或目录服务发布。
- **TBD（Phase 6+）**：DID 解析机制——是通过去中心化注册表（did:web / did:ion）还是 P2P gossip？

### 2.2 Agent 身份
- Agent 在实例内由 `agent_id`（内核盖章）标识。
- 跨 OS 通信时，外交工具为 Agent 签发**代理身份**：`<instance_did>/agents/<agent_id>`，避免暴露内部 `agent_id`（防关联攻击）。

### 2.3 可验证凭证 (VC)
- 实例能力声明以 VC 表达：`VC = { issuer: instance_did, subject: agent_id, claims: Capability[], expiration, signature }`。
- 能力类型示例：
  - "本实例可提供文件传输服务"（File Channel SEND/RECV）
  - "本实例可转发 AI 模型推理 API"（API Channel + 模型指纹）
  - "本实例承诺不使用 <敏感数据类别>"

### 2.4 跨 OS 身份互认
- 首次接触：互相验证 DID Document 签名 → 评估信誉值 → 决定握手。
- **TBD（Phase 6+）**：信誉模型——本地维护的信誉表 vs 上链的分布式信誉。

---

## 3. 协议层 (Protocol)

外交工具需兼容业界现有协议族（原始构想调研列表）：

| 协议 | 定位 | 关系 |
|------|------|------|
| **A2A**（Agent-to-Agent，Google → Linux 基金会） | Agent ↔ Agent 跨框架通信 | **首选主协议**（JSON-RPC 2.0 + SSE） |
| **MCP**（Model Context Protocol，Anthropic） | Agent ↔ 工具/数据 | 适配为外交工具内部能力调用接口 |
| **ATH**（Agent Trusted Handshake，中国信通院） | 信任层（身份互验、权限管控、行为审计） | 握手时强制走 ATH |
| **AIP**（国家标准 GB/Z 185-2026） | 智能体互联全生命周期 | 国内合规场景必须 |
| **AP2**（Agent Payment） | 授权支付合约 | 经济化场景预留 |
| **ACP**（Agent Communication Protocol，AgentUnion） | 异步消息 + 离线发现 | 补充通道 |

### 3.1 信封格式（首期草案，Phase 6 收口）
```jsonc
// 外交通用信封，跨所有协议族
{
  "v": "diplomat/1.0",                 // 协议版本
  "from": "did:synapse:abc...",        // 源实例 DID
  "to":   "did:synapse:xyz...",        // 目标实例 DID
  "agent": "did:synapse:abc.../agents/foo",  // 发起 Agent
  "channel": "api",                    // ChannelKind
  "credential": "<SD-JWT 短期凭证>",    // 见 §2.3
  "behavior_contract_id": "uuid",      // 本次行为契约 ID（见 §5）
  "intent": "summarize_webpage",       // 高层意图（用户态可读）
  "payload_ref": "<content_hash>",     // 业务对象引用（详见各通道编解码器）
  "expires_at_ns": 1234567890,
  "signature": "..."                   // 外交工具私钥签名
}
```

### 3.2 协议路由策略
- 收到入站外交请求 → 校验信封签名 + DID 解析 + 凭证合法性 → 路由到对应 Channel 处理。
- 不同协议族的私有字段通过 adapter 适配到内部 Channel 接口；不修改核心外交逻辑。

---

## 4. 短期动态凭证 (Short-lived Credentials)

### 4.1 凭证生命周期
```
[Agent 发起外交请求]
    ↓ 外交工具生成短期凭证
[凭证 = SD-JWT (subject=agent_id, channel, scope, ttl=任务时长)]
    ↓ 嵌入信封
[跨 OS 传输]
    ↓ 目标外交工具校验
[任务结束 → 凭证失效 → 不可重用]
```

### 4.2 凭证撤销
- 任务结束、Agent 崩溃、行为围栏触发 → 外交工具立即撤销凭证，发布到本地 CRL（凭证吊销列表）。
- **TBD**：跨 OS CRL 同步机制（Phase 6+）。

### 4.3 与 Capability 模型的关系
- 短期凭证 = **本实例对外的 capability 投影**（文档 01 的 Capability → VC 映射）。
- 接收方只信 VC，不直接信对端 Agent 的 `agent_id`（防伪造）。

---

## 5. 行为契约与信誉 (Behavioral Contracts & Reputation)

原始构想："信誉 + 契约 + 惩罚"是 AI OS 时代的"法律体系"。本节定义最小可行版本。

### 5.1 契约模型
```rust
// 跨 OS 行为契约草案
pub struct BehaviorContract {
    pub id: ContractId,
    pub parties: [Did; 2],
    pub scope: Vec<ContractClause>,        // 契约条款列表
    pub duration: Duration,
    pub penalty: PenaltyPolicy,            // 违约处置
}

pub enum ContractClause {
    NoCrossBorderData(String),             // "不跨境传输 <数据类型>"
    MaxResources(int) / Minor("memory_mb"), // "内存占用 ≤ N MB"
    NoCodeExec,                            // "不执行任意代码"
    PrivacyPreserving,                     // "启用差分隐私"
    // ... 可扩展
}
```

### 5.2 信誉模型
- 每个对端 DID 维护本地信誉值 `reputation ∈ [-100, 100]`：
  - 完成契约 +1；违规 -10；严重违规 -50 + 临时封禁。
- 信誉值进入握手策略：`reputation < -50` → 拒绝握手；`X` 阈值 → 要求额外校验。
- **TBD（Phase 6+）**：是否引入跨实例信誉共享（同联盟内可信第三方审计）？

### 5.3 违约检测与惩罚
- 接收方外交工具监控对方行为是否偏离契约条款（基于 §7 行为围栏）。
- 触发违约 → 立即生成 `BreachReport` 凭证 → 通告违约方 → 扣减信誉 → 切断会话 → 必要时上报联盟。
- **TBD**：自动仲裁机制（Phase 6+），引入可信第三方或去中心化仲裁合约。

---

## 6. 冲突消解 (Conflict Resolution)

当两个 AI OS 外交行为产生冲突时的处置策略：

| 策略 | 适用场景 | 实现 |
|------|---------|------|
| **协商** | 双方资源/能力冲突 | 多轮对话（带最大轮数上限防死循环） |
| **仲裁** | 不可调和 | 引入可信第三方或预设 Supervisor 角色 |
| **优先级** | 任务优先级明确 | 安全 > 隐私 > 性能 > 经济 |
| **置信度机制** | 同任务多方案分歧 | 输出附带置信度，差异大时重试或人工介入 |
| **市场机制** | 资源竞争 | 拍卖 / 竞价（预留，远期） |

---

## 7. 行为围栏与伦理规范

外交工具内置硬编码**伦理红线**（原始构想引用国内规范）：

| 红线 | 处置 |
|------|------|
| 传输禁止数据（PII/密钥/商业机密）| 阻断 + 审计 + 信誉清零 |
| 参与伤害性协作 | 拒绝 + 上报联盟 |
| 未透明可释（决策不可追溯）| 拒绝握手 |
| 违反隐私法规（过度采集 / 跨境）| 阻断 + 审计 |
| 强制可解释性 | 所有外交决策可审计查询 |

伦理红线在外交工具编译期硬编码，**不依赖 Agent 自律**。

---

## 8. 模式：被动响应 vs 主动协商

原始构想提出之问，本节给出倾向答案：

| 模式 | 描述 | 适用范围 |
|------|------|---------|
| **被动响应** | 外交工具仅响应他方请求 | 高风险、首次接触方 |
| **主动协商** | 外交工具按策略主动发起 | 已知合作方、资源调度、能力发现 |
| **分层混合（推荐）** | 低风险主动协商 / 高风险必须经 Agent 发起 + 用户确认 | 日常使用 |

> **TBD（Phase 6+）**：具体分层阈值与人工确认通道（UI 提示？语音？）。

---

## 9. 性能与跨 OS 延迟

跨 OS 通信相对本机 IPC 多 100~1000 倍延迟（网络往返），设计须考虑：

- 大对象**禁止走外交协议**，应通过协商建立共享存储层（联邦存储 / DHT）后再传输引用。
- 流式交互优先用 SSE / WebSocket 而非请求-响应。
- 短期凭证校验开销应低于 100 µs（用本地缓存 + 异步撤销推送）。

---

## 10. 与设计文档 04 的边界

| 关注点                                              | [文档 04](04-diplomat-channel-architecture.md) | 文档 05（本文档）|
| --------------------------------------------------- | ---------------------------------------------- | ---------------- |
| 唯一网络出口不变量                                  | ✅                                             | 复用             |
| 5 类 Channel（File/Web/Stream/Api/Realtime）        | ✅                                             | 复用             |
| 入出站安全扫描                                      | ✅                                             | 复用             |
| 协议适配（A2A/MCP/ATH）                             | ❌                                             | ✅               |
| 跨 OS 身份互认                                      | ❌                                             | ✅               |
| 信誉与契约                                          | ❌                                             | ✅               |
| 冲突消解                                            | ❌                                             | ✅               |

简言之：**文档 04 = 内部架构；文档 05 = 对外交互**。两者共享同一个外交工具进程实现。

---

## 11. 待决策清单（Phase 6+ 启动时再细化）

- [ ] DID 解析机制：去中心化注册表 vs P2P gossip
- [ ] 信誉模型：本地 vs 联盟共享
- [ ] 跨 OS CRL 同步
- [ ] 自动仲裁机制（可信第三方 / 链上合约）
- [ ] 主动协商 / 被动响应的分层阈值
- [ ] 大对象跨 OS 共享存储方案（联邦存储 / IPFS / 自研）
- [ ] 协议版本协商细节（信封 v 字段语义）
- [ ] 与 [设计文档 04](04-diplomat-channel-architecture.md) 的 v1 协议兼容性测试策略

---

## 12. 参考 (References)

- W3C DID Core 1.0 / VC Data Model 2.0
- IETF SD-JWT（Selective Disclosure JWT）draft
- A2A Protocol（Linux Foundation）
- MCP Specification（Anthropic）
- 中国信通院 ATH（Agent Trusted Handshake）协议
- GB/Z 185-2026 智能体互联系列国家标准
- 《人工智能应用伦理安全指引 1.0》
- 《生成式人工智能服务管理暂行办法》