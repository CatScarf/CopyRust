# 规范

## 通用规范

### 代码规范

- 通用代码规范
  - 新增代码规范
    - 代码注释不要过于冗余，参考Linus风格，只保留必要注释，注释用英文除非用户明确要求用其他语言
    - 函数行数尽量不超过50行，强烈避免超过100行
    - 文件行数尽量不超过 500行，强烈避免超过 1000行
    - 一个模块下文件和文件夹合计不超过 15 个，如果超过你可以继续分子模块
    - 函数和变量尽可能属于某个类或struct，裸露的变量和函数不要太多
    - 每个函数必须有单行的注释，除非用户要求否则不要写 Args, Return 等注释，用一行说明函数功能即可，注释要用英文，**但其他规范有明确授权时遵循其他规范**
  - 现存代码规范
    - 当修改的代码是我们独立或主导的项目时，可以按照前面的新增代码规范清理过去的屎山
    - 当修改的代码是其他人写或上游原本就有的时候，不要强行遵循上述规范，不要随便动其他人或上游的代码。只在我们新增或修改的地方这样做就行
- Rust规范
  - 使用 rustfmt 统一格式，并通过 `cargo fmt --all -- --check` 确认工作区格式一致.
  - 使用 `cargo clippy --workspace --all-targets --all-features -- -D warnings` 执行严格静态检查，所有 warning 均视为错误.
  - 严禁使用 `unwrap`，错误必须向上传播，且不能忽略，且每次传播错误必须附带自己的信息，例如 `<Failed to xxx: <内层信息>>`，这样一旦出错就能看出来堆栈，但可以使用 `unwrap_or` 处理预期内问题.
  - 如果一句 use 的函数/变量/struct总数量超过5个，则只use他们的父mod即可，调用时通过::形式调用，避免过多的use.
  - 为保证编译速度，一个Crate不得超过100个文件
  - Rust日志必须使用tracing写日志，除非必要不使用原始的println，打印的日志必须包含时间戳.
- Python规范
  - Python 版本以项目配置为准；使用当前 Works 工具链验证时，优先与项目声明的目标版本保持一致
  - 使用 Black 统一格式化，并通过 Black 的检查模式确认改动文件已格式化
  - 使用 Pyright 严格类型检查，新增和修改代码应达到零错误、零警告；项目同时配置 Mypy 时，两项类型检查都必须通过
  - 函数参数、返回值、公共类属性和模块级数据结构应提供准确类型注解，不使用宽泛类型或强制转换掩盖真实类型问题
  - 使用 pytest 或项目现有测试框架覆盖改动范围；提交前至少完成任务级功能验证，不能以语法检查替代实际行为验证
  - 只格式化本次改动涉及的文件，避免引入无关的全文件格式差异
  - 预期内的失败应转换为明确的异常或错误结果，并保留必要的上下文；不得静默吞掉异常、丢弃错误结果或用默认值掩盖失败

### 版本控制规范

- Perforce规范
  - 执行P4命令时，必须先用登记点的p4port/p4user/workspace，然后用p4 -c client -p port -u user xxx作为前缀执行P4命令，否则本地有多个工作区时会发生混乱.
  - 所有 Perforce changelist 的描述必须使用中文，包括 pending、shelved 和 submitted CL
  - P4全库更新这种长任务也可以考虑用定时监控以避免浪费Token
  - 不要使用独立的工作区和Workspace，代价非常高，就用当前的工作区即可。但仅做少数文件的合并操作时例外。当你发现工作区有其他文件打开时可以判断下是否影响当前任务不影响就不需要动留在那就行
- Git规范
  - 每次提交前必须检查 Git Diff、待提交文件和未跟踪文件，不得直接批量提交未经审查的内容
  - 单个文件超过 1 MB（1,048,576 字节）时不得提交，**但其他规范有明确授权时遵循其他规范**，不区分文件类型；发现后应移出暂存区，并按用途删除、放入对应任务的 `temp` 目录或改用外部制品存储
  - 不得提交工具的编译、运行或打包产物，包括但不限于 `target`、`Binaries`、`Intermediate`、`Saved`、`DerivedDataCache`、`__pycache__`、测试缓存、可执行文件、调试符号、对象文件和可再生成的归档
  - 不得提交临时或无意义文件，包括但不限于日志、错误输出、临时文件、空占位文件、临时压缩包、一次性探针目录和重复生成的中间报告
  - 工具产物和临时文件应放入对应任务的 `temp` 目录并由 Git 忽略；发现新的通用产物类型时应同步补充 `.gitignore`

### 其他通用规范

- 环境规范
  - 执行检查、构建或验证命令时如果提示命令不存在，可以先确认项目配置和推荐安装方式；在不破坏现有环境的前提下，允许安装一份所需工具后继续验证
- Unreal项目规范
  - 调试 UE 编辑器/Cook 包时，优先使用 UE 的 MCP 或 Automation，仅在没有其他可行方案时使用 Computer Use
  - MCP端口和是否开放会记录在 `<Game>\Config\DefaultEditorPerProjectUserSettings.ini`，你可以优先查阅该文件以确认是否启用。如果MCP未启用或启用失败，你可以修改此ini然后重启
  - 通常来讲，用户不会有未保存但需要保留的内容，当用户要求你用编辑器操作时，如果此时没有AI任务正在使用编辑器，则现有未保存的内容不需要保存也不需要恢复
  - 启动编辑器时，不得额外添加指定分辨率的参数（如 `-ResX`、`-ResY`）
  - 使用 UE 编辑器执行自动化性能跑测前，必须先关闭失焦节流，可在编辑器 Cmd 控制台执行 `py import unreal; unreal.get_default_object(unreal.EditorPerformanceSettings).set_editor_property("bThrottleCPUWhenNotForeground", False)`
- Codex规范
  - 控制本机应用时，优先使用专用工具、API 或命令行，仅在 Computer Use 是唯一可行方案时使用；浏览器内的 Computer Use 不受此限制，访问网站时优先使用 Chrome + Computer Use 而不是内置浏览器
  - 撰写文本时，如果用户新加了约束或要求，不要在文档或代码里体现，不要表述为因为用户要求怎么做而怎么做，不要写“因为用户要求XXX，所以XXX”，不要写“不是XXX，所以要XXX”。将最终约束视为一个整体。不要像叛逆的学生一样老师提了要求就在作业里写因为老师的要求所以XXX
  - 当你使用computeruse却被用户终止后，视为用户禁止你使用computeruse，此时不得终止任务必须采取其他方案

当你开一个空的工程时，你需要新建AGENTS.md并将以上通用规范拷贝过，**但其他规范有明确授权时遵循其他规范**


## CopyRust

- Product documentation and code comments use English. Work records belong in the Works task directory.
- Preserve independent Rust library, CLI, and typed Python packages with one shared copy engine.
- Verify correctness and benchmark against robocopy before publishing. Never include benchmark data or build outputs in Git.
