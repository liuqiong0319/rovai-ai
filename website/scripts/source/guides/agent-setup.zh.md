# 智能体接入与注意事项

各智能体的安装入口，以及账号、模型与工具配置的差异。Rovai 界面中的操作和字段说明见[智能体与模型配置](https://rovai.dev/zh/docs/agents.html)。

本文以 **Rovai v0.4.7** 为基础；在 Rovai 中选择 Pi 思考强度需要 **v0.4.8 或更新版本**。安装和登录在运行 Rovai Desktop 或 Server 的主机上完成。

在原生智能体中完成账号与模型配置后，到「设置 → 智能体」确认 Rovai 能找到它，再为队员选择智能体与模型。已有的登录和配置继续使用；通过浏览器访问时，使用的是服务主机上的安装。

## 支持的智能体 {#supported}

| 智能体 · 官方安装说明 | 本机入口 |
| --- | --- |
| [Codex CLI](https://github.com/openai/codex) | `codex` |
| [Claude Code](https://code.claude.com/docs/en/setup) | `claude` |
| [Pi Coding Agent](https://github.com/earendil-works/pi) | `pi` |
| [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness) | `dsh` |
| [OpenCode](https://opencode.ai/docs/) | `opencode` |
| [GitHub Copilot CLI](https://docs.github.com/en/copilot/how-tos/copilot-cli/set-up-copilot-cli/install-copilot-cli) | `copilot` |
| [Kiro CLI](https://kiro.dev/docs/cli/) | `kiro-cli` |
| [Qoder CLI](https://docs.qoder.com/cli/quick-start) | `qodercli` |
| [CodeBuddy CLI](https://www.codebuddy.ai/docs/cli/quickstart) | `codebuddy` |
| [Qwen Code](https://github.com/QwenLM/qwen-code) | `qwen` |
| [TRAE CLI CN](https://www.trae.cn/) | `traecli` |
| [Kimi Code](https://github.com/MoonshotAI/kimi-cli) | `kimi` |
| [Grok Build](https://docs.x.ai/build/overview) | `grok` |
| [ZCode](https://zcode.z.ai/en/docs/install) | 原生应用 |
| [Antigravity](https://antigravity.google/) | 应用附带的 CLI |

表中链接指向各产品的官方安装说明。Pi 需要 0.84.4 或更高的兼容版本，DSH 需要 0.1.5-rc.2 或更高的兼容版本，Grok 需要 1.0.0 或更高的兼容版本。Intel Mac 上的 ZCode 仍为预览；各系统的可用范围见[支持平台与智能体](https://rovai.dev/zh/docs/compatibility.html)。

## DSH：自定义模型与思考强度 {#dsh}

运行 `dsh web`，在 **Settings → Models** 中添加 Provider，填写 API 协议、地址、凭据和模型。使用标准 Web Profile 时，Rovai 可以补充读取这里配置的 `llm-pi-ai` 模型；回到队员配置中选择即可。两边需要使用同一个 `DSH_HOME`，未设置时为 `~/.dsh`。

### 为自定义模型声明思考档位

DSH 的模型选择器只展示模型已声明的档位。自带的 DeepSeek 模型已有 off / low / high / max；手动添加的模型通常没有这些声明，因此模型已经能用，却没有思考强度选项。

在 DSH 设置页使用 **Open configuration file** 打开当前配置。标准 `dsh web` 对应 `~/.dsh/profiles/web/cordis.patch.yml`；设置了 `DSH_HOME` 或使用自定义 Profile 时，路径随之变化。

找到 `llm-pi-ai` 的 `config.providers.<你的 Provider>.models`，在**已有的目标模型**下补充 `reasoningEfforts`。下面展示模型项的写法，保留该模型其他字段、其他模型与 Provider 配置：

```yaml
- id: your-model-id
  reasoningEfforts:
    off:
    low: low
    medium: medium
    high: high
    xhigh: xhigh
```

- 左侧是可选档位，右侧是传给上游的值。只保留该模型和 API 支持的档位；例如 `max: xhigh` 可把名为 max 的档位映射为 xhigh。
- `off:` 留空表示不发送强度参数，**不保证上游停止思考**。默认会思考的模型可能需要对应的兼容字段，例如 DeepSeek 的 `compat.thinkingFormat`。
- 保存后重新打开模型选择器。具体协议、档位与默认值的含义见 [DSH 官方模型配置说明](https://github.com/deepseek-ai/deepseek-harness/blob/master/docs/user/guide/providers.md#reasoning-effort)和 [llm-pi-ai 配置参考](https://github.com/deepseek-ai/deepseek-harness/blob/master/packages/llm/llm-pi-ai/README.md)。

### 回到 Rovai 选择参数

在 Rovai 中选中**具体模型**，再查看它的模型参数。思考档位来自这一个 Provider + Model 的原生返回；切换模型后，选项也可能不同。“模型默认”沿用 DSH 的默认行为。

DSH Web 的默认模型与 ACP 入口的默认模型分别处理。想固定队员使用某个自定义模型时，在 Rovai 中显式选择并保存。修改配置后，重新选择模型或重开参数页会重新核对能力；如果读取失败，界面中的诊断与重试入口可用于定位问题。

### Web 配置的适用范围

Rovai v0.4.7 补充读取的是**标准 Web Profile** 的模型配置；同名 Provider 已有原生配置时优先使用原生配置。[dsh-model-thinking](https://github.com/cyberlieflife/dsh-model-thinking) 是第三方 Web 插件，添加后形成的自定义 Web bundle 不在这条补充路径的支持范围内。这份示例使用原生配置文件方式，完整边界见 [DSH 接入说明](https://github.com/murray17/rovai-ai/blob/v0.4.7/docs/architecture/runtime-catalog-boundaries.md#deepseek-harness-acp)。

## Codex 与 Claude Code 的账号、模型服务 {#native-settings}

这两种智能体沿用自己的登录、Provider 和模型配置。Rovai 中的「启动设置」负责程序路径与子进程环境；账号、Key 和网关地址在对应智能体中管理。

同时保留多个安装版本或配置目录时，Rovai 的程序路径和环境需要对应你已经登录的那一份。终端里能够运行、Rovai 却找不到程序时，可以在启动设置中填写可执行文件的绝对路径。

## Pi：模型与思考强度配置 {#pi}

### 配置模型

使用 Pi 内置 Provider 时，在 Pi 终端界面运行 `/login` 登录，再用 `/model` 选择模型。自定义服务（如 sub2api）在运行 Rovai 的主机上配置 `~/.pi/agent/models.json`。

下面以 GPT-6.1 Sol 为例。替换网关地址和 API Key；已有配置时，合并对应的 Provider、模型字段，保留其他内容。

```json
{
  "providers": {
    "sub2api": {
      "api": "openai-responses",
      "baseUrl": "https://gateway.example/v1",
      "apiKey": "YOUR_API_KEY",
      "models": [{
        "id": "gpt-6.1-sol",
        "reasoning": true,
        "thinkingLevelMap": {
          "off": null, "minimal": null,
          "low": "low", "medium": "medium",
          "high": "high", "xhigh": "xhigh",
          "max": "max"
        }
      }]
    }
  }
}
```

### 配置思考强度

`reasoning: true` 声明模型支持思考；`thinkingLevelMap` 的左侧是 Pi 档位，右侧是传给上游的值，`null` 表示不提供该档位。上例提供 **low / medium / high / xhigh / max** 五档，其他模型按其实际支持情况填写。

保存后在 Pi 中重新打开 `/model` 加载配置，也可用 `/thinking` 选择强度。回到 Rovai 的**队员 → 运行配置**，打开模型选择器完成刷新，选择具体模型和思考强度，再保存。选择“跟随模型默认值”时，Rovai 不额外指定强度。

Pi 命令以 1.1.0 为例。[Pi 模型配置说明](https://github.com/earendil-works/pi/blob/v1.1.0/packages/coding-agent/docs/models.md) · [GPT-6.1 Sol 档位说明](https://developers.openai.com/api/docs/models/gpt-6.1-sol)

## 接入新智能体 {#adding-agents}

Rovai 通过原生结构化协议或 ACP 对接编程智能体。新增适配涉及会话、执行事件、模型、工具与权限等能力；现有实现、代码入口和当前缺口汇总在[智能体接入](https://github.com/murray17/rovai-ai/blob/main/contributing/agent-integrations.zh-CN.md)。

[本地开发](https://github.com/murray17/rovai-ai/blob/main/docs/development/README.md) · [接入 Checklist](https://github.com/murray17/rovai-ai/blob/main/docs/development/runtime-integration-checklist.md)
