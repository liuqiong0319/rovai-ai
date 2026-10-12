---
document_type: contract
name: Runtime Launch and Verification
version: v60
status: accepted
source_version: v1.72
last_updated: 2026-10-12
---

# Runtime Launch and Verification v60

继承 [v59](runtime-launch-and-verification-v59.md) 的进程启动环境、Warm、权限、健康和输入保护。
以下规则替代历史配置相等的原生恢复资格、Bootstrap 历史依赖与按错误文本推断不可恢复。

## 恢复目标与执行保护

当前 Conversation 的原生 Session ID 只属于当前 Camp、当前队员和当前会话。未主动重置且已有绑定的
AdapterKind 与本次 Runtime 相同，就交给该 Adapter 的原生恢复方式尝试；不以安装 ID、路径、版本、
指纹、安装代次、连接/权限/上下文摘要或历史兼容 key 不同提前返回 New。旧记录从当前绑定的安装记录
及对应历史 Run 取得类型；无法确定或相互矛盾时明确报错，不查找其他会话替代。

主动重置、没有 Session ID 或切换 CLI 类型时新建。配置与历史不相等不等于原生历史无效；当前程序、
协议、工具、工作目录和权限仍必须可用，不能用旧权限执行。权限无法落实等错误失败本次执行，保留旧目标。
恢复成功按现有绑定事务更新运行元数据，不能伪造摘要相等或绕过 CAS、Run/epoch 和会话归属。

历史 controlled-resume 的 attempting/ambiguous/incompatible 不再永久阻止后续新输入恢复上下文。
现有活跃执行、Session 锁、停止/清理、unknown outcome、输入接收及执行代次仍独立生效；允许续接历史
不授权重新提交已接收或结果未知的业务输入，也不授权跨 Camp/队员复用进程。

## Adapter 结论与一次降级

结论在对应 Adapter 的恢复调用处产生，上层只消费成功、明确不可恢复、其他错误。复用已有错误类型，
不新增分类数据库或恢复账本。明确不可恢复只接受：原生能力无恢复方式、该恢复请求的已定义结构化拒绝，
或已有精确目标核验确认目标不存在/历史无法加载。Command Code 保留完整目录的 exact ID/cwd 核验；
目录不完整、分页或损坏不证明目标消失。Pi 既有完整 locator/目标文件的确定不存在可作为证据。

Pi locator 在既有 Camp/队员私有目录内按完整原生 Session UUID 分文件，公聊与单聊不能互相覆盖。
旧单文件 locator 只在请求的完整 ID 匹配时作为回读来源；已有该 Session 的独立 locator 优先，错配不作为历史丢失证据。
不扫描或猜测其他原生历史来替换目标。

ACP 保留既有 `AcpRpcError` 的机器字段与脱敏诊断至请求调用处。只有实际 `session/resume` / `session/load`
返回的 `-32601` 表明该恢复方法不可用；若还有广告且 Adapter 支持的 load 方式，先以相同 ID 尝试一次。
全部可用恢复方式均明确不可用时才给出降级结论；初始化、模型、MCP 或目录请求的同一码不获得该结论。

没有稳定机器证据的 CLI 按未知错误处理。不能对 error.to_string()、stderr 或日志匹配 incompatible、
not found、method not found 等文本；普通内部/参数错误、进程退出、超时、取消、认证、网络或临时锁
不触发降级。初始化、模型设置和 MCP 请求失败不能冒充会话恢复失败。

明确不可恢复且当前 Run/epoch 未取消、未 dispatch、没有 accepted/unknown 输入时，允许走现有流程
新建一次。新建失败保留旧原生目标、不提示成功；成功激活并完成正常绑定后才切换目标。预备工具凭据
可以按现有代次轮换，但不能提前删除旧恢复目标。降级不自动重试新建，也不重放先前输入。

成功替换沿用原有 continuity 事件，continuation 为 native_session_replaced。公开/私聊执行状态只
对该精确 Run/epoch 的成功事件显示一次说明：“原生会话未能恢复，已新建会话继续。Camp 中的消息保留。”
不显示成恢复成功，不弹确认；单队员失败不阻断其他 Camp。

## 本次上下文与历史审计

Bootstrap、章程、提示词及上下文格式/版本差异均不参与恢复资格，也不触发新会话。新 Run 使用正常
上下文装配与各 Adapter 既有交付方式；旧原生历史允许保留旧说明。历史 Bootstrap 仅供对应历史输入审计，
不要求读取、修复、迁移或对齐其内容才能续聊。

同一 Run/Binding/generation 的已准备 Bootstrap 与 Manifest 保持冻结，输入幂等和 accepted/unknown
保护不变。后续 Run 在现有 Evidence 表保存本次正常装配结果；Migration 195/schema 145 只移除该表
对 Binding/generation 的唯一约束、保留非唯一索引和所有历史行/Blob 引用，不读取旧 Blob。
这不引入历史重建、内容一致性审核或新的上下文版本门槛。
