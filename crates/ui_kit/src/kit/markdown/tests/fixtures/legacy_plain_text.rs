pub const LEGACY_PLAIN_TEXT: &[(&str, &str)] = &[
    (
        "en:common.market_not_loaded",
        "This widget needs the metadata of a supported market participant to be loaded first.\nLoad the metadata of at least one supported market participant from the  button in the top left corner.\nThe market participants this widget supports are:\n%{markets}",
    ),
    (
        "en:settings_ui.market_participants.clear_credentials_message",
        "Clear the saved credentials in \"%{title}\"? Trading features that use them will stop working until they are filled in again.",
    ),
    (
        "en:settings_ui.proxies.delete_message",
        "Delete the proxy \"%{name}\"? Network targets set to this proxy go back to the default proxy.",
    ),
    (
        "en:settings_ui.save_failed.message",
        "The settings were not written to disk: %{reason}",
    ),
    (
        "en:startup.config_reset_message",
        "The configuration file was damaged and has been reset to defaults. The previous file was kept as %{file}.",
    ),
    (
        "en:startup.config_reset_message_no_backup",
        "The configuration file was damaged and has been reset to defaults. The previous file could not be backed up.",
    ),
    (
        "en:widget.candlestick.select_instrument_prompt",
        "Select an instrument from the picker in the top left",
    ),
    (
        "en:widget.coin_market_cap_fear_and_greed_index.logged_out",
        "You need to register or log in to use this feature.",
    ),
    (
        "en:widget.cryptocurrency_etf.logged_out",
        "You need to register or log in to use this feature.",
    ),
    (
        "en:widget.empty.hint",
        "Click the  button in the upper-right corner to select a widget.",
    ),
    (
        "en:widget.ethereum_wallet.empty_addresses",
        "No monitored wallet addresses",
    ),
    (
        "en:widget.ethereum_wallet.logged_out",
        "You need to register or log in to use this feature.",
    ),
    (
        "en:widget.financial_calendar.logged_out",
        "You need to register or log in to use this feature.",
    ),
    (
        "en:widget.financial_index.logged_out",
        "You need to register or log in to use this feature.",
    ),
    (
        "en:widget.financial_index.select_at_least_one",
        "Select at least one index",
    ),
    (
        "en:widget.global_shipping_chokepoint.sign_in",
        "You need to register or log in to see the vessels in the strait.",
    ),
    (
        "en:widget.hyperliquid_wallet.empty_addresses",
        "No monitored wallets yet — add one from the settings control in the top-left corner.",
    ),
    (
        "en:widget.market_trade.select_instrument_prompt",
        "Select an instrument from the picker in the top left",
    ),
    (
        "en:widget.news.logged_out",
        "You need to register or log in to use this feature.",
    ),
    (
        "en:widget.order_book.select_instrument_prompt",
        "Select an instrument from the picker in the top left",
    ),
    (
        "en:widget.pentagon_pizza_index.logged_out",
        "You need to register or log in to use this feature.",
    ),
    (
        "en:widget.perpetual_statistics.select_instrument_prompt",
        "Select a perpetual instrument from the picker in the top left",
    ),
    (
        "en:widget.prediction_market.select_event_prompt",
        "Select a prediction event",
    ),
    (
        "en:widget.solana_wallet.empty_addresses",
        "No monitored wallet addresses",
    ),
    (
        "en:widget.solana_wallet.logged_out",
        "You need to register or log in to use this feature.",
    ),
    (
        "en:widget.watch_list.market_metadata_not_loaded",
        "Load the metadata of %{market}  first",
    ),
    ("en:widget.x.add_success", "Account added successfully"),
    (
        "en:widget.x.logged_out",
        "You need to register or log in to use this feature.",
    ),
    (
        "en:window.account.authorization_complete_message",
        "Authorized wallet %{kind} %{wallet}",
    ),
    (
        "en:window.account.awaiting_authorization",
        "Finish signing in and authorizing in your browser. Choosing “Cancel sign-in” gives up this attempt.",
    ),
    (
        "en:window.account.sign_out_confirm",
        "Signing out means authorizing again before account features can be used.",
    ),
    ("en:window.workspace.add_message", "Name the new workspace."),
    (
        "en:window.workspace.delete_message",
        "Delete the workspace \"%{name}\"? Its pane layout is deleted with it.",
    ),
    (
        "en:window.workspace.name_exists",
        "Another workspace already uses this name",
    ),
    (
        "en:window.workspace.name_required",
        "Enter a workspace name",
    ),
    (
        "en:window.workspace.rename_message",
        "Enter the new name of \"%{name}\".",
    ),
    (
        "zh:common.market_not_loaded",
        "当前部件需要先加载受支持的市场参与者元数据才可以使用。\n请先从左上角的  按钮加载至少一个受支持的市场参与者元数据。\n本部件支持的市场参与者有：\n%{markets}",
    ),
    (
        "zh:error.financial_index.duplicate_category",
        "指数清单包含重复分类 %{code}",
    ),
    (
        "zh:error.financial_index.duplicate_index",
        "指数清单包含重复指数 %{code}",
    ),
    (
        "zh:error.financial_index.unknown_category",
        "指数 %{index} 引用了未定义的分类 %{category}",
    ),
    (
        "zh:settings_ui.market_participants.clear_credentials_message",
        "确定清除“%{title}”中已保存的凭据吗？在重新填写前，相关交易功能将无法使用。",
    ),
    (
        "zh:settings_ui.proxies.delete_message",
        "确定删除代理「%{name}」吗？指定为该代理的网络目标会回到默认代理。",
    ),
    (
        "zh:settings_ui.save_failed.message",
        "设置未能写入磁盘：%{reason}",
    ),
    (
        "zh:startup.config_reset_message",
        "配置文件受损，已重新初始化为默认配置。原文件已备份为 %{file}。",
    ),
    (
        "zh:startup.config_reset_message_no_backup",
        "配置文件受损，已重新初始化为默认配置；原文件备份失败。",
    ),
    (
        "zh:widget.candlestick.select_instrument_prompt",
        "请从左上角的标的物选择器中选择标的",
    ),
    (
        "zh:widget.coin_market_cap_fear_and_greed_index.logged_out",
        "需要注册/登陆后才可以使用",
    ),
    (
        "zh:widget.cryptocurrency_etf.logged_out",
        "需要注册/登陆后才可以使用",
    ),
    ("zh:widget.empty.hint", "请点击右上角的  按钮选择一个部件"),
    (
        "zh:widget.ethereum_wallet.empty_addresses",
        "暂无监控钱包地址",
    ),
    (
        "zh:widget.ethereum_wallet.logged_out",
        "需要注册/登陆后才可以使用",
    ),
    (
        "zh:widget.financial_calendar.logged_out",
        "需要注册或登录后才能使用该功能。",
    ),
    (
        "zh:widget.financial_index.logged_out",
        "需要注册/登陆后才可以使用",
    ),
    (
        "zh:widget.financial_index.select_at_least_one",
        "请选择至少一个指数",
    ),
    (
        "zh:widget.global_shipping_chokepoint.sign_in",
        "注册或登录后即可查看海峡内的船舶。",
    ),
    (
        "zh:widget.hyperliquid_wallet.empty_addresses",
        "暂无监控钱包，请从左上角设置中添加",
    ),
    (
        "zh:widget.market_trade.select_instrument_prompt",
        "请从左上角的标的物选择器中选择标的",
    ),
    ("zh:widget.news.logged_out", "需要注册/登陆后才可以使用"),
    (
        "zh:widget.order_book.select_instrument_prompt",
        "请从左上角的标的物选择器中选择标的",
    ),
    (
        "zh:widget.pentagon_pizza_index.logged_out",
        "需要注册/登陆后才可以使用",
    ),
    (
        "zh:widget.perpetual_statistics.select_instrument_prompt",
        "请从左上角的标的物选择器中选择永续合约标的",
    ),
    (
        "zh:widget.prediction_market.select_event_prompt",
        "请选择一个预测事件",
    ),
    (
        "zh:widget.solana_wallet.empty_addresses",
        "暂无监控钱包地址",
    ),
    (
        "zh:widget.solana_wallet.logged_out",
        "需要注册/登陆后才可以使用",
    ),
    (
        "zh:widget.watch_list.market_metadata_not_loaded",
        "请先加载  %{market} 的元数据",
    ),
    ("zh:widget.x.add_success", "账号添加成功"),
    ("zh:widget.x.logged_out", "需要注册/登陆后才可以使用"),
    (
        "zh:window.account.authorization_complete_message",
        "已授权钱包 %{kind} %{wallet}",
    ),
    (
        "zh:window.account.awaiting_authorization",
        "请在浏览器中完成登录/授权流程。点击“取消登录”可放弃本次登录。",
    ),
    (
        "zh:window.account.sign_out_confirm",
        "退出登录后需要重新授权才能使用账户功能。",
    ),
    ("zh:window.workspace.add_message", "为新工作区命名。"),
    (
        "zh:window.workspace.delete_message",
        "确定删除工作区「%{name}」吗？其中的面板布局也会一并删除。",
    ),
    (
        "zh:window.workspace.name_exists",
        "该名称已被其它工作区使用",
    ),
    ("zh:window.workspace.name_required", "请输入工作区名称"),
    (
        "zh:window.workspace.rename_message",
        "输入「%{name}」的新名称。",
    ),
    (
        "en:prod:widget.watch_list.market_metadata_not_loaded",
        "Load the metadata of Binance  first",
    ),
    (
        "en:prod:settings_ui.proxies.delete_message",
        "Delete the proxy \"Example Proxy\"? Network targets set to this proxy go back to the default proxy.",
    ),
    (
        "zh:prod:widget.watch_list.market_metadata_not_loaded",
        "请先加载  币安 的元数据",
    ),
    (
        "zh:prod:settings_ui.proxies.delete_message",
        "确定删除代理「Example Proxy」吗？指定为该代理的网络目标会回到默认代理。",
    ),
    (
        "release:en",
        "Kytrix v0.0.3\n\nAdded\n\nCandlestick indicators\n\nCandlestick charts now support technical indicators. A new Chart settings button in the header, to the left of the adjustment and interval selectors, opens a settings panel on the right side of the chart and narrows the chart area.\n\nThe main chart supports MA and EMA, and the sub charts support VOL, MACD, RSI and KDJ. Several sub charts can be enabled at once and stack from top to bottom in the order VOL → MACD → RSI → KDJ. For MA, EMA and RSI, each line's period (1–500) and colour can be set, and lines can be added or removed individually; MACD has short, long and signal periods, with separate toggles and colours for DIF and DEA, and also draws a histogram; KDJ has a calculation period and two smoothing periods, with separate toggles and colours for K, D and J; VOL has no settings.\n\nIndicator colours are set with a new colour picker: choose a swatch from the palette, adjust the HSLA sliders, or type a hexadecimal colour. Each indicator's enabled state, settings and colours are saved with the workspace; VOL and MA (5, 10, 20) are enabled by default. The top-left corner of the main chart and of each sub chart shows the name, parameters and current value of each line or indicator, and moving the crosshair reveals the value at the corresponding bar. When you enable an indicator that needs more history, the chart fetches the required history automatically.\n\nGlobal Shipping Chokepoint\n\nThe new Global Shipping Chokepoint widget tracks vessels in the Strait of Hormuz using live AIS data. The basemap is drawn from OpenStreetMap coastlines, borders and roads, with separate colour schemes for the light and dark appearances, and names for 5 countries, 31 cities and 2 bodies of water (some city names appear as you zoom in).\n\nThe map supports drag-to-pan and wheel or pinch zoom (1×–4×). Vessels are coloured by 20 ship types, with the bow direction following their course; hovering over a vessel shows 16 fields, including its name, MMSI, IMO, call sign, flag, ship type, navigation status, speed, course, heading, destination, ETA, draught, dimensions, and the last update times for dynamic and static data. The header can filter by ship type; the legend keeps every type visible (types excluded by the filter are greyed out), and the vessel count reflects the current filter. If the data has not updated for more than 5 minutes, the widget reports the current picture as failed and offers a recovery action instead of continuing to present it as live data.\n\nThe widget requires sign-in, and the account must include an AIS vessel data subscription; without one, you can view subscription plans from the widget.\n\nFinancial Calendar\n\nThe new Financial Calendar widget lists upcoming global financial events by date. The table has four columns — Importance, Time, Country and Name — and events are grouped by day; importance is shown as dots (one to three for low, medium and high respectively), and the event's reference period (month, quarter and so on) appears beside its name.\n\nThe header can filter by importance, date and country, and reset all filters with one click. All times are shown in the Data time zone setting, and changing the time zone re-groups events by day immediately; all-day events are never shifted to an adjacent date. The calendar refreshes automatically every 10 minutes, with a countdown to the next refresh and a manual refresh button in the header. The widget requires sign-in.\n\nPentagon Pizza Index\n\nThe new Pentagon Pizza Index widget shows the latest observation of the index. The index is a five-level readiness scale, from Level 1 (\"Maximum readiness\") to Level 5 (\"Lowest state of readiness\"); the card is coloured by level and shows the level number and observation time. The refresh button in the header shows a countdown to the next automatic reading (every 10 minutes) and can also refresh immediately. The widget requires sign-in.\n\nCoinMarketCap Fear & Greed Index\n\nThe new CoinMarketCap Fear & Greed Index widget uses a semicircular gauge to show the current index value and its level (Extreme Fear, Fear, Neutral, Greed or Extreme Greed), with historical values for yesterday, last week, last month and last year listed below, as well as the yearly high and low over the past 365 calendar days, the dates on which they occurred and the levels at the time. The widget refreshes automatically every 60 seconds and can also be refreshed manually. The widget requires sign-in.\n\nChanged\n\nWallet monitoring split\n\nThe former Wallet Monitor has been split into two independent widgets, Ethereum Wallet Monitor and Solana Wallet Monitor, each using the address format and monitoring API for its chain. The wallet address list no longer shows Active/Paused status or update times, and is sorted by when each address was added; the management dialog is now chain-specific.\n\nSettings navigation\n\nThe settings window now has a navigation sidebar on the left and page content on the right: group headings no longer collapse, and pages are listed directly beneath them. Search rebuilds the sidebar as you type, keeping only matching pages and groups, and automatically switches to the first matching page when the current page no longer matches.\n\nUpdate dialog\n\nThe app update entry point is now an icon-only button. The update dialog has been redesigned: release notes are shown in a wider panel with a persistent scrollbar, download status and progress are centred, and the old dialog title and hover tooltip have been removed.\n\nWidget names and categories\n\nTweet Monitor has been renamed to X; ETF Flow has been renamed to Crypto Currency ETF and moved from the Market category to Statistics; Hyperliquid Wallet Monitor has been renamed to Hyperliquid Wallet.\n\nMemory use\n\nLoading the full market catalogues for several venues now retains about 57 MiB of heap instead of about 125 MiB, and the load peak falls from about 143 MiB to about 90 MiB. Refreshing an already-loaded venue no longer keeps a full copy of the old catalogue, so repeated refreshes no longer accumulate memory.\n\nFixed\n\nCandlestick history\n\nWhen a chart follows the live edge, widening the window or zooming out until more than one page of candles is visible no longer leaves a blank strip on the left that cannot be filled. History is now prefetched continuously as the viewport needs it, and prefetching no longer evicts the latest candle you are viewing.\n\nmacOS title bar\n\nOn macOS, the entire height of the custom title bar can now be used to drag the window; previously only the upper part worked. Double-clicking the title bar maximises or restores the window, and clicking tabs or buttons no longer starts an accidental drag.\n\nSingular forms\n\nIn the English interface, count-dependent text now uses the singular form when the value is 1, so wording such as \"1 wallets\" or \"1 indices\" no longer appears.",
    ),
    (
        "release:zh",
        "Kytrix v0.0.3\n\n新增\n\nK 线指标\n\nK 线图现在支持技术指标。标题栏右侧新增「图表设置」按钮（位于复权与周期选择左侧），打开后在图表右侧展开设置面板，图表区域随之收窄。\n\n主图支持 MA 与 EMA，副图支持 VOL、MACD、RSI 与 KDJ。可同时启用多个副图，按 VOL → MACD → RSI → KDJ 的顺序自上而下排列。MA、EMA 与\nRSI 的指标线可逐条设置周期（1–500）和颜色，并可新增或删除；MACD 可设置短周期、长周期与移动平均周期，分别控制 DIF、DEA\n的显示和颜色，并绘制柱状图；KDJ 可设置计算周期与两个移动平均周期，分别控制 K、D、J 的显示和颜色；VOL 无需设置。\n\n指标颜色由新的颜色选择器设置：可从色板挑选，也可用 HSLA 滑杆调整，或直接输入十六进制颜色。所有指标的启用状态、参数与颜色都会随工作区保存；默认启用\nVOL 与 MA (5, 10, 20)。主图与各副图左上角会列出指标线或指标的名称、参数与当前值，十字光标移动时同步显示对应柱位的数值。启用需要更多历史数据的指标时，图表会自动补取所需历史。\n\n全球关键航道\n\n新增「全球关键航道」组件，以实时 AIS 数据监控霍尔木兹海峡的船舶。底图使用 OpenStreetMap 的海岸线、国界与道路绘制，亮色与暗色外观各有一套配色，并标注\n5 个国家、31 个城市与 2 个水域的名称（部分城市名随缩放显示）。\n\n地图支持拖动平移，以及滚轮与双指缩放（1×–4×）。船舶按 20\n种船型着色，船头方向跟随航向；将指针移到船舶上可查看船名、MMSI、IMO、呼号、船旗国、船型、航行状态、航速、航向、艏向、目的港、预计到达、吃水、尺寸，以及动态与静态数据更新时间等\n16 项信息。标题栏可按船型筛选，图例保留全部船型（被筛掉的类型置灰），船舶数量显示当前筛选结果。数据超过 5\n分钟未更新时，组件会把当前画面报告为失败并提供恢复入口，而不是继续把它当作实时数据。\n\n该组件需要登录，且账户需包含 AIS 船舶数据订阅；没有订阅时可从组件内查看订阅方案。\n\n财经日历\n\n新增「财经日历」组件，按日期列出未来的全球财经事件。表格分为重要性、时间、国家与名称四列，事件按天分组；重要性以圆点数量表示（低、中、高分别为一至三个），名称旁会附带事件对应的月份、季度等参考期。\n\n标题栏可按重要性、日期与国家筛选，并可一键重置。所有时间按「数据时间时区」显示，切换时区后日期分组会同步重算；全天事件不会被换算到相邻日期。日历每\n10 分钟自动刷新，标题栏同时显示距离下次刷新的倒计时和手动刷新按钮。该组件需要登录后使用。\n\n五角大楼披萨指数\n\n新增「五角大楼披萨指数」组件，展示该指数的最新观测。指数以五级战备等级表示，从等级 1「最高戒备」到等级\n5「最低戒备」；卡片按等级着色，并显示等级编号与观测时间。标题栏的刷新按钮会显示距离下一次自动读取的倒计时（每 10\n分钟自动读取一次），也可以立即刷新。该组件需要登录后使用。\n\nCoinMarketCap 恐惧贪婪指数\n\n新增「CoinMarketCap 恐惧贪婪指数」组件：半圆仪表显示当前指数值与所处档位（极度恐惧、恐惧、中性、贪婪、极度贪婪），下方列出昨天、上周、上月与去年的历史值，以及过去\n365 个日历日的年度最高/最低值、发生日期与当时的档位。组件每 60 秒自动刷新，也可手动刷新。该组件需要登录后使用。\n\n变更\n\n钱包监控拆分\n\n原「钱包监控」拆分为「Ethereum 钱包监控」与「Solana\n钱包监控」两个独立组件，各自使用对应链的地址格式和监控接口。钱包地址列表不再显示「已激活 / 已暂停」状态与更新时间，改为按添加时间排序；管理对话框也按链区分。\n\n设置界面\n\n设置窗口改为左侧导航栏加右侧内容页：分组标题不再折叠，页面直接列在标题下；搜索会实时重建设置栏，只保留命中的页面与分组，并在当前页不再匹配时自动切到第一个匹配页面。\n\n更新对话框\n\n应用更新入口改为图标按钮；更新对话框重新布局，发布说明使用更宽的面板并带常驻滚动条，下载状态与进度居中显示，旧有的对话框标题与悬浮提示已移除。\n\n组件名称与分类\n\n「推文监控」更名为「X」；「ETF 资金流」更名为「加密货币ETF」，并从「行情」分类移入「统计」分类；「Hyperliquid 钱包监控」更名为「Hyperliquid\n钱包」。\n\n内存占用\n\n加载多个场所的完整行情目录时，常驻内存由约 125 MiB 降至约 57 MiB，加载峰值由约 143 MiB 降至约 90\nMiB；刷新已加载的场所不再保留整份旧目录，反复刷新也不会持续累积内存。\n\n修复\n\nK 线历史数据\n\n图表跟随最新行情时，窗口加宽，或将 K 线缩放到一屏显示超过一页后，左侧不再留下无法填充的空白；现在会按视口需要连续预取历史，并且预取不会挤掉正在查看的最新一根\nK 线。\n\nmacOS 标题栏\n\nmacOS 上自定义标题栏的整个高度现在都可以拖动窗口，此前只有上半部分有效；双击标题栏可最大化或还原，点击标签页与按钮也不会误触发拖动。\n\n数量文案\n\n英文界面中，数值为 1 的计数文案改用单数形式，不再出现「1 wallets」「1 indices」等写法。",
    ),
];

pub const LEGACY_PLAIN_TEXT_FOLDED: &[(&str, &str)] = &[
    (
        "release:en",
        "Kytrix v0.0.3\nAdded\nCandlestick indicators\nCandlestick charts now support technical indicators. A new Chart settings button in the header, to the left of the adjustment and interval selectors, opens a settings panel on the right side of the chart and narrows the chart area.\nThe main chart supports MA and EMA, and the sub charts support VOL, MACD, RSI and KDJ. Several sub charts can be enabled at once and stack from top to bottom in the order VOL → MACD → RSI → KDJ. For MA, EMA and RSI, each line's period (1–500) and colour can be set, and lines can be added or removed individually; MACD has short, long and signal periods, with separate toggles and colours for DIF and DEA, and also draws a histogram; KDJ has a calculation period and two smoothing periods, with separate toggles and colours for K, D and J; VOL has no settings.\nIndicator colours are set with a new colour picker: choose a swatch from the palette, adjust the HSLA sliders, or type a hexadecimal colour. Each indicator's enabled state, settings and colours are saved with the workspace; VOL and MA (5, 10, 20) are enabled by default. The top-left corner of the main chart and of each sub chart shows the name, parameters and current value of each line or indicator, and moving the crosshair reveals the value at the corresponding bar. When you enable an indicator that needs more history, the chart fetches the required history automatically.\nGlobal Shipping Chokepoint\nThe new Global Shipping Chokepoint widget tracks vessels in the Strait of Hormuz using live AIS data. The basemap is drawn from OpenStreetMap coastlines, borders and roads, with separate colour schemes for the light and dark appearances, and names for 5 countries, 31 cities and 2 bodies of water (some city names appear as you zoom in).\nThe map supports drag-to-pan and wheel or pinch zoom (1×–4×). Vessels are coloured by 20 ship types, with the bow direction following their course; hovering over a vessel shows 16 fields, including its name, MMSI, IMO, call sign, flag, ship type, navigation status, speed, course, heading, destination, ETA, draught, dimensions, and the last update times for dynamic and static data. The header can filter by ship type; the legend keeps every type visible (types excluded by the filter are greyed out), and the vessel count reflects the current filter. If the data has not updated for more than 5 minutes, the widget reports the current picture as failed and offers a recovery action instead of continuing to present it as live data.\nThe widget requires sign-in, and the account must include an AIS vessel data subscription; without one, you can view subscription plans from the widget.\nFinancial Calendar\nThe new Financial Calendar widget lists upcoming global financial events by date. The table has four columns — Importance, Time, Country and Name — and events are grouped by day; importance is shown as dots (one to three for low, medium and high respectively), and the event's reference period (month, quarter and so on) appears beside its name.\nThe header can filter by importance, date and country, and reset all filters with one click. All times are shown in the Data time zone setting, and changing the time zone re-groups events by day immediately; all-day events are never shifted to an adjacent date. The calendar refreshes automatically every 10 minutes, with a countdown to the next refresh and a manual refresh button in the header. The widget requires sign-in.\nPentagon Pizza Index\nThe new Pentagon Pizza Index widget shows the latest observation of the index. The index is a five-level readiness scale, from Level 1 (\"Maximum readiness\") to Level 5 (\"Lowest state of readiness\"); the card is coloured by level and shows the level number and observation time. The refresh button in the header shows a countdown to the next automatic reading (every 10 minutes) and can also refresh immediately. The widget requires sign-in.\nCoinMarketCap Fear & Greed Index\nThe new CoinMarketCap Fear & Greed Index widget uses a semicircular gauge to show the current index value and its level (Extreme Fear, Fear, Neutral, Greed or Extreme Greed), with historical values for yesterday, last week, last month and last year listed below, as well as the yearly high and low over the past 365 calendar days, the dates on which they occurred and the levels at the time. The widget refreshes automatically every 60 seconds and can also be refreshed manually. The widget requires sign-in.\nChanged\nWallet monitoring split\nThe former Wallet Monitor has been split into two independent widgets, Ethereum Wallet Monitor and Solana Wallet Monitor, each using the address format and monitoring API for its chain. The wallet address list no longer shows Active/Paused status or update times, and is sorted by when each address was added; the management dialog is now chain-specific.\nSettings navigation\nThe settings window now has a navigation sidebar on the left and page content on the right: group headings no longer collapse, and pages are listed directly beneath them. Search rebuilds the sidebar as you type, keeping only matching pages and groups, and automatically switches to the first matching page when the current page no longer matches.\nUpdate dialog\nThe app update entry point is now an icon-only button. The update dialog has been redesigned: release notes are shown in a wider panel with a persistent scrollbar, download status and progress are centred, and the old dialog title and hover tooltip have been removed.\nWidget names and categories\nTweet Monitor has been renamed to X; ETF Flow has been renamed to Crypto Currency ETF and moved from the Market category to Statistics; Hyperliquid Wallet Monitor has been renamed to Hyperliquid Wallet.\nMemory use\nLoading the full market catalogues for several venues now retains about 57 MiB of heap instead of about 125 MiB, and the load peak falls from about 143 MiB to about 90 MiB. Refreshing an already-loaded venue no longer keeps a full copy of the old catalogue, so repeated refreshes no longer accumulate memory.\nFixed\nCandlestick history\nWhen a chart follows the live edge, widening the window or zooming out until more than one page of candles is visible no longer leaves a blank strip on the left that cannot be filled. History is now prefetched continuously as the viewport needs it, and prefetching no longer evicts the latest candle you are viewing.\nmacOS title bar\nOn macOS, the entire height of the custom title bar can now be used to drag the window; previously only the upper part worked. Double-clicking the title bar maximises or restores the window, and clicking tabs or buttons no longer starts an accidental drag.\nSingular forms\nIn the English interface, count-dependent text now uses the singular form when the value is 1, so wording such as \"1 wallets\" or \"1 indices\" no longer appears.",
    ),
    (
        "release:zh",
        "Kytrix v0.0.3\n新增\nK 线指标\nK 线图现在支持技术指标。标题栏右侧新增「图表设置」按钮（位于复权与周期选择左侧），打开后在图表右侧展开设置面板，图表区域随之收窄。\n主图支持 MA 与 EMA，副图支持 VOL、MACD、RSI 与 KDJ。可同时启用多个副图，按 VOL → MACD → RSI → KDJ 的顺序自上而下排列。MA、EMA 与\nRSI 的指标线可逐条设置周期（1–500）和颜色，并可新增或删除；MACD 可设置短周期、长周期与移动平均周期，分别控制 DIF、DEA\n的显示和颜色，并绘制柱状图；KDJ 可设置计算周期与两个移动平均周期，分别控制 K、D、J 的显示和颜色；VOL 无需设置。\n指标颜色由新的颜色选择器设置：可从色板挑选，也可用 HSLA 滑杆调整，或直接输入十六进制颜色。所有指标的启用状态、参数与颜色都会随工作区保存；默认启用\nVOL 与 MA (5, 10, 20)。主图与各副图左上角会列出指标线或指标的名称、参数与当前值，十字光标移动时同步显示对应柱位的数值。启用需要更多历史数据的指标时，图表会自动补取所需历史。\n全球关键航道\n新增「全球关键航道」组件，以实时 AIS 数据监控霍尔木兹海峡的船舶。底图使用 OpenStreetMap 的海岸线、国界与道路绘制，亮色与暗色外观各有一套配色，并标注\n5 个国家、31 个城市与 2 个水域的名称（部分城市名随缩放显示）。\n地图支持拖动平移，以及滚轮与双指缩放（1×–4×）。船舶按 20\n种船型着色，船头方向跟随航向；将指针移到船舶上可查看船名、MMSI、IMO、呼号、船旗国、船型、航行状态、航速、航向、艏向、目的港、预计到达、吃水、尺寸，以及动态与静态数据更新时间等\n16 项信息。标题栏可按船型筛选，图例保留全部船型（被筛掉的类型置灰），船舶数量显示当前筛选结果。数据超过 5\n分钟未更新时，组件会把当前画面报告为失败并提供恢复入口，而不是继续把它当作实时数据。\n该组件需要登录，且账户需包含 AIS 船舶数据订阅；没有订阅时可从组件内查看订阅方案。\n财经日历\n新增「财经日历」组件，按日期列出未来的全球财经事件。表格分为重要性、时间、国家与名称四列，事件按天分组；重要性以圆点数量表示（低、中、高分别为一至三个），名称旁会附带事件对应的月份、季度等参考期。\n标题栏可按重要性、日期与国家筛选，并可一键重置。所有时间按「数据时间时区」显示，切换时区后日期分组会同步重算；全天事件不会被换算到相邻日期。日历每\n10 分钟自动刷新，标题栏同时显示距离下次刷新的倒计时和手动刷新按钮。该组件需要登录后使用。\n五角大楼披萨指数\n新增「五角大楼披萨指数」组件，展示该指数的最新观测。指数以五级战备等级表示，从等级 1「最高戒备」到等级\n5「最低戒备」；卡片按等级着色，并显示等级编号与观测时间。标题栏的刷新按钮会显示距离下一次自动读取的倒计时（每 10\n分钟自动读取一次），也可以立即刷新。该组件需要登录后使用。\nCoinMarketCap 恐惧贪婪指数\n新增「CoinMarketCap 恐惧贪婪指数」组件：半圆仪表显示当前指数值与所处档位（极度恐惧、恐惧、中性、贪婪、极度贪婪），下方列出昨天、上周、上月与去年的历史值，以及过去\n365 个日历日的年度最高/最低值、发生日期与当时的档位。组件每 60 秒自动刷新，也可手动刷新。该组件需要登录后使用。\n变更\n钱包监控拆分\n原「钱包监控」拆分为「Ethereum 钱包监控」与「Solana\n钱包监控」两个独立组件，各自使用对应链的地址格式和监控接口。钱包地址列表不再显示「已激活 / 已暂停」状态与更新时间，改为按添加时间排序；管理对话框也按链区分。\n设置界面\n设置窗口改为左侧导航栏加右侧内容页：分组标题不再折叠，页面直接列在标题下；搜索会实时重建设置栏，只保留命中的页面与分组，并在当前页不再匹配时自动切到第一个匹配页面。\n更新对话框\n应用更新入口改为图标按钮；更新对话框重新布局，发布说明使用更宽的面板并带常驻滚动条，下载状态与进度居中显示，旧有的对话框标题与悬浮提示已移除。\n组件名称与分类\n「推文监控」更名为「X」；「ETF 资金流」更名为「加密货币ETF」，并从「行情」分类移入「统计」分类；「Hyperliquid 钱包监控」更名为「Hyperliquid\n钱包」。\n内存占用\n加载多个场所的完整行情目录时，常驻内存由约 125 MiB 降至约 57 MiB，加载峰值由约 143 MiB 降至约 90\nMiB；刷新已加载的场所不再保留整份旧目录，反复刷新也不会持续累积内存。\n修复\nK 线历史数据\n图表跟随最新行情时，窗口加宽，或将 K 线缩放到一屏显示超过一页后，左侧不再留下无法填充的空白；现在会按视口需要连续预取历史，并且预取不会挤掉正在查看的最新一根\nK 线。\nmacOS 标题栏\nmacOS 上自定义标题栏的整个高度现在都可以拖动窗口，此前只有上半部分有效；双击标题栏可最大化或还原，点击标签页与按钮也不会误触发拖动。\n数量文案\n英文界面中，数值为 1 的计数文案改用单数形式，不再出现「1 wallets」「1 indices」等写法。",
    ),
];

pub const LEGACY_DOCUMENT_PLAIN_TEXT: &[(&str, &str, &str, &str)] = &[
    (
        "document:plain_text_reads_blocks_lines_and_inline_marks",
        "# title\n\na  \nb![x](file://x.png)c",
        "title\n\na\nbc",
        "title\na\nbc",
    ),
    (
        "document:plain_text_reads_code_quote_list_and_table_blocks",
        "```rust\nlet x = 1;\n```\n\n> quoted\n\n2. one\n\n3. two\n\n---\n\n| h1 | h2 |\n|---|---|\n| a | b |\n",
        "let x = 1;\n\nquoted\n\none\n\ntwo\n\nh1 h2\na b",
        "let x = 1;\nquoted\none\ntwo\nh1 h2\na b",
    ),
];

pub const PRODUCTION_PLAIN_TEXT_SOURCES: &[(&str, &str)] = &[
    (
        "en:prod:widget.watch_list.market_metadata_not_loaded",
        "<p align=\"center\">Load the metadata of **Binance** ![](file://brands/Binance/binance.png?size=16) first</p>",
    ),
    (
        "en:prod:settings_ui.proxies.delete_message",
        "<p align=\"center\">Delete the proxy \"Example Proxy\"? Network targets set to this proxy go back to the default proxy.</p>",
    ),
    (
        "zh:prod:widget.watch_list.market_metadata_not_loaded",
        "<p align=\"center\">请先加载 ![](file://brands/Binance/binance.png?size=16) **币安** 的元数据</p>",
    ),
    (
        "zh:prod:settings_ui.proxies.delete_message",
        "<p align=\"center\">确定删除代理「Example Proxy」吗？指定为该代理的网络目标会回到默认代理。</p>",
    ),
];
