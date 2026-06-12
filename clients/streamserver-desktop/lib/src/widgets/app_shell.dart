import 'dart:async';
import 'dart:io' show Platform;

import 'package:flutter/material.dart';
import 'package:lucide_icons_flutter/lucide_icons.dart';
import 'package:window_manager/window_manager.dart';

import '../core/theme/stream_theme.dart';
import '../state.dart';
import '../utils.dart';
import 'data_panel.dart';
import 'embedded_player_panel.dart';

class AppShell extends StatelessWidget {
  const AppShell({
    required this.current,
    required this.onNavigate,
    required this.child,
    super.key,
  });

  final AppSection current;
  final ValueChanged<AppSection> onNavigate;
  final Widget child;

  @override
  Widget build(BuildContext context) {
    final controller = AppScope.of(context);
    return LayoutBuilder(
      builder: (context, constraints) {
        final width = constraints.maxWidth;
        final compact = width < 900;
        final showInspector = width >= 1180 &&
            current != AppSection.overview &&
            controller.inspectorVisible;
        return Scaffold(
          backgroundColor: context.streamColors.appBackground,
          drawer: compact
              ? Drawer(
                  width: 280,
                  child: _Sidebar(
                    current: current,
                    onNavigate: (section) {
                      Navigator.of(context).pop();
                      onNavigate(section);
                    },
                  ),
                )
              : null,
          body: Column(
            children: [
              const _WindowChromeBar(),
              Expanded(
                child: Row(
                  children: [
                    if (!compact)
                      SizedBox(
                        width: width < 1180 ? 236 : 260,
                        child:
                            _Sidebar(current: current, onNavigate: onNavigate),
                      ),
                    Expanded(
                      child: Column(
                        children: [
                          _AutoRefreshTicker(controller: controller),
                          _TopCommandBar(
                            compact: compact,
                            current: current,
                            controller: controller,
                            onNavigate: onNavigate,
                          ),
                          if (compact)
                            _CompactTabs(
                                current: current, onNavigate: onNavigate),
                          Expanded(
                            child: Row(
                              children: [
                                Expanded(
                                  child: _MainContent(
                                    controller: controller,
                                    compact: compact,
                                    child: child,
                                  ),
                                ),
                                if (showInspector)
                                  _RightInspector(
                                    current: current,
                                    controller: controller,
                                  ),
                              ],
                            ),
                          ),
                        ],
                      ),
                    ),
                  ],
                ),
              ),
            ],
          ),
        );
      },
    );
  }
}

class _WindowChromeBar extends StatelessWidget {
  const _WindowChromeBar();

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    final brightness = Theme.of(context).brightness;
    final isMacOS = Platform.isMacOS;
    return Container(
      height: 34,
      decoration: BoxDecoration(
        color: colors.sidebar,
        border: Border(
          bottom: BorderSide(color: Colors.white.withValues(alpha: 0.08)),
        ),
      ),
      child: Row(
        children: [
          Expanded(
            child: DragToMoveArea(
              child: Padding(
                padding: EdgeInsets.only(left: isMacOS ? 76 : 16),
                child: Row(
                  children: [
                    Icon(LucideIcons.circlePlay,
                        color: colors.primary, size: 15),
                    const SizedBox(width: 8),
                    const Text(
                      'StreamServer 控制台',
                      maxLines: 1,
                      overflow: TextOverflow.ellipsis,
                      style: TextStyle(
                        color: Colors.white,
                        fontSize: 13,
                        fontWeight: FontWeight.w800,
                      ),
                    ),
                  ],
                ),
              ),
            ),
          ),
          if (!isMacOS) ...[
            WindowCaptionButton.minimize(
              brightness: brightness,
              onPressed: () => windowManager.minimize(),
            ),
            FutureBuilder<bool>(
              future: windowManager.isMaximized(),
              builder: (context, snapshot) {
                if (snapshot.data == true) {
                  return WindowCaptionButton.unmaximize(
                    brightness: brightness,
                    onPressed: () => windowManager.unmaximize(),
                  );
                }
                return WindowCaptionButton.maximize(
                  brightness: brightness,
                  onPressed: () => windowManager.maximize(),
                );
              },
            ),
            WindowCaptionButton.close(
              brightness: brightness,
              onPressed: () => windowManager.close(),
            ),
          ],
        ],
      ),
    );
  }
}

class _MainContent extends StatelessWidget {
  const _MainContent({
    required this.controller,
    required this.compact,
    required this.child,
  });

  final AppController controller;
  final bool compact;
  final Widget child;

  @override
  Widget build(BuildContext context) {
    return Column(
      children: [
        if (controller.activeMediaUrl != null)
          Padding(
            padding: EdgeInsets.fromLTRB(
                compact ? 12 : 24, 14, compact ? 12 : 18, 0),
            child: EmbeddedPlayerPanel(
              url: controller.activeMediaUrl!,
              title: controller.activeMediaTitle,
            ),
          ),
        Expanded(
          child: SingleChildScrollView(
            padding: EdgeInsets.fromLTRB(
              compact ? 14 : 24,
              compact ? 14 : 22,
              compact ? 14 : 18,
              24,
            ),
            child: child,
          ),
        ),
      ],
    );
  }
}

class _TopCommandBar extends StatelessWidget {
  const _TopCommandBar({
    required this.compact,
    required this.current,
    required this.controller,
    required this.onNavigate,
  });

  final bool compact;
  final AppSection current;
  final AppController controller;
  final ValueChanged<AppSection> onNavigate;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    return SafeArea(
      bottom: false,
      child: Container(
        height: compact ? 72 : 86,
        padding: EdgeInsets.symmetric(horizontal: compact ? 12 : 28),
        decoration: BoxDecoration(
          color: alpha(colors.surface, context.isDarkMode ? 0.82 : 0.92),
          border: Border(bottom: BorderSide(color: colors.border)),
        ),
        child: Row(
          children: [
            if (compact)
              IconButton(
                tooltip: '导航',
                onPressed: () => Scaffold.of(context).openDrawer(),
                icon: const Icon(LucideIcons.panelLeft),
              ),
            Expanded(
              child: _PageTitle(section: current),
            ),
            if (!compact) ...[
              const SizedBox(width: 18),
              const _CommandSearch(),
            ],
            const SizedBox(width: 14),
            _AutoRefresh(controller: controller),
            if (!compact && current != AppSection.overview)
              IconButton(
                tooltip: controller.inspectorVisible ? '隐藏运行摘要' : '显示运行摘要',
                onPressed: controller.toggleInspectorVisible,
                icon: Icon(
                  controller.inspectorVisible
                      ? LucideIcons.panelRightClose
                      : LucideIcons.panelRightOpen,
                  size: 19,
                ),
              ),
            IconButton(
              tooltip: '刷新当前页',
              onPressed: () => controller.notifyCurrentView(),
              icon: const Icon(LucideIcons.refreshCw, size: 19),
            ),
            IconButton(
              tooltip: '切换主题',
              onPressed: controller.toggleThemeMode,
              icon: Icon(
                controller.themeMode == ThemeMode.dark
                    ? LucideIcons.sun
                    : LucideIcons.moon,
                size: 19,
              ),
            ),
            _NotificationButton(colors: colors),
            const SizedBox(width: 6),
            _UserMenu(controller: controller),
          ],
        ),
      ),
    );
  }
}

class _PageTitle extends StatelessWidget {
  const _PageTitle({required this.section});

  final AppSection section;

  @override
  Widget build(BuildContext context) {
    final item = navItemFor(section);
    final colors = context.streamColors;
    return Column(
      mainAxisAlignment: MainAxisAlignment.center,
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Text(
          item.label,
          maxLines: 1,
          overflow: TextOverflow.ellipsis,
          style: TextStyle(
            color: colors.textPrimary,
            fontSize: 22,
            fontWeight: FontWeight.w800,
            height: 1.1,
          ),
        ),
        const SizedBox(height: 6),
        Text(
          item.note,
          maxLines: 1,
          overflow: TextOverflow.ellipsis,
          style: TextStyle(color: colors.textSecondary, fontSize: 13),
        ),
      ],
    );
  }
}

class _CommandSearch extends StatelessWidget {
  const _CommandSearch();

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    return SizedBox(
      width: 320,
      height: 42,
      child: TextField(
        decoration: InputDecoration(
          hintText: '搜索任务 / 节点 / 文件...',
          prefixIcon: const Icon(LucideIcons.search, size: 18),
          suffixIcon: Container(
            width: 38,
            alignment: Alignment.center,
            margin: const EdgeInsets.all(8),
            decoration: BoxDecoration(
              border: Border.all(color: colors.border),
              borderRadius: BorderRadius.circular(6),
            ),
            child: Text(
              '⌘K',
              style: TextStyle(
                color: colors.textSecondary,
                fontSize: 11,
                fontWeight: FontWeight.w700,
              ),
            ),
          ),
        ),
      ),
    );
  }
}

class _AutoRefresh extends StatelessWidget {
  const _AutoRefresh({required this.controller});

  final AppController controller;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    return Row(
      mainAxisSize: MainAxisSize.min,
      children: [
        Icon(LucideIcons.refreshCw, size: 15, color: colors.textSecondary),
        const SizedBox(width: 7),
        Text('自动刷新', style: TextStyle(color: colors.textSecondary)),
        const SizedBox(width: 8),
        DropdownButtonHideUnderline(
          child: DropdownButton<int>(
            value: controller.autoRefreshSeconds,
            borderRadius: BorderRadius.circular(8),
            items: const [
              DropdownMenuItem(value: 0, child: Text('关闭')),
              DropdownMenuItem(value: 5, child: Text('5s')),
              DropdownMenuItem(value: 10, child: Text('10s')),
              DropdownMenuItem(value: 30, child: Text('30s')),
            ],
            onChanged: (value) {
              if (value != null) controller.setAutoRefreshSeconds(value);
            },
          ),
        ),
      ],
    );
  }
}

class _AutoRefreshTicker extends StatefulWidget {
  const _AutoRefreshTicker({required this.controller});

  final AppController controller;

  @override
  State<_AutoRefreshTicker> createState() => _AutoRefreshTickerState();
}

class _AutoRefreshTickerState extends State<_AutoRefreshTicker> {
  Timer? _timer;
  int _seconds = -1;

  @override
  void initState() {
    super.initState();
    _syncTimer();
  }

  @override
  void didUpdateWidget(covariant _AutoRefreshTicker oldWidget) {
    super.didUpdateWidget(oldWidget);
    _syncTimer();
  }

  @override
  void dispose() {
    _timer?.cancel();
    super.dispose();
  }

  void _syncTimer() {
    final seconds = widget.controller.autoRefreshSeconds;
    if (_seconds == seconds) return;
    _seconds = seconds;
    _timer?.cancel();
    _timer = null;
    if (seconds <= 0) return;
    _timer = Timer.periodic(Duration(seconds: seconds), (_) {
      widget.controller.notifyCurrentView();
    });
  }

  @override
  Widget build(BuildContext context) => const SizedBox.shrink();
}

class _NotificationButton extends StatefulWidget {
  const _NotificationButton({required this.colors});

  final StreamColors colors;

  @override
  State<_NotificationButton> createState() => _NotificationButtonState();
}

class _NotificationButtonState extends State<_NotificationButton> {
  int? _failedTasks;
  int _loadedSeed = -1;

  @override
  void initState() {
    super.initState();
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (mounted) _load();
    });
  }

  @override
  void didChangeDependencies() {
    super.didChangeDependencies();
    final seed = AppScope.of(context).viewRefreshSeed;
    if (_loadedSeed >= 0 && _loadedSeed != seed) Future.microtask(_load);
  }

  Future<void> _load() async {
    final controller = AppScope.of(context);
    final seed = controller.viewRefreshSeed;
    try {
      final page = await controller.api(
        'GET',
        '/api/v1/tasks',
        query: {'page_size': 1, 'status': 'FAILED'},
      );
      final map = (page as Map).cast<String, Object?>();
      if (!mounted) return;
      setState(() {
        _failedTasks =
            (map['total'] as num?)?.toInt() ?? rowsFrom(map['items']).length;
        _loadedSeed = seed;
      });
    } catch (_) {
      if (!mounted) return;
      setState(() {
        _failedTasks = null;
        _loadedSeed = seed;
      });
    }
  }

  @override
  Widget build(BuildContext context) {
    final count = _failedTasks ?? 0;
    return Stack(
      clipBehavior: Clip.none,
      children: [
        IconButton(
          tooltip: '通知',
          onPressed: _showNotifications,
          icon: const Icon(LucideIcons.bell, size: 19),
        ),
        if (count > 0)
          Positioned(
            top: 8,
            right: 8,
            child: IgnorePointer(
              child: Container(
                padding: const EdgeInsets.symmetric(horizontal: 5, vertical: 1),
                decoration: BoxDecoration(
                  color: widget.colors.danger,
                  borderRadius: BorderRadius.circular(999),
                ),
                child: Text(
                  count > 99 ? '99+' : '$count',
                  style: const TextStyle(
                    color: Colors.white,
                    fontSize: 10,
                    fontWeight: FontWeight.w800,
                  ),
                ),
              ),
            ),
          ),
      ],
    );
  }

  Future<void> _showNotifications() async {
    final count = _failedTasks ?? 0;
    await showDialog<void>(
      context: context,
      builder: (context) {
        final colors = context.streamColors;
        return AlertDialog(
          title: const Text('通知'),
          content: SizedBox(
            width: 360,
            child: Column(
              mainAxisSize: MainAxisSize.min,
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                if (count > 0)
                  Text('当前有 $count 个失败任务需要处理。')
                else
                  const Text('当前没有需要处理的任务通知。'),
                const SizedBox(height: 12),
                Text(
                  '通知数据来自任务接口，会随顶部自动刷新同步更新。',
                  style: TextStyle(color: colors.textSecondary, fontSize: 12),
                ),
              ],
            ),
          ),
          actions: [
            TextButton(
              onPressed: () => Navigator.of(context).pop(),
              child: const Text('关闭'),
            ),
            if (count > 0)
              FilledButton.icon(
                onPressed: () {
                  Navigator.of(context).pop();
                  AppScope.of(context).navigate(AppSection.tasks);
                },
                icon: const Icon(LucideIcons.listChecks, size: 16),
                label: const Text('查看任务'),
              ),
          ],
        );
      },
    );
  }
}

class _UserMenu extends StatelessWidget {
  const _UserMenu({required this.controller});

  final AppController controller;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    return PopupMenuButton<String>(
      tooltip: '用户菜单',
      onSelected: (value) {
        if (value == 'logout') controller.logout();
      },
      itemBuilder: (context) => const [
        PopupMenuItem(value: 'logout', child: Text('退出登录')),
      ],
      child: Row(
        children: [
          CircleAvatar(
            radius: 17,
            backgroundColor: colors.primary,
            child: Text(
              controller.subject.isEmpty
                  ? 'A'
                  : controller.subject.characters.first.toUpperCase(),
              style: const TextStyle(
                color: Colors.white,
                fontWeight: FontWeight.w800,
              ),
            ),
          ),
          if (MediaQuery.sizeOf(context).width >= 1040) ...[
            const SizedBox(width: 9),
            ConstrainedBox(
              constraints: const BoxConstraints(maxWidth: 116),
              child: Column(
                mainAxisSize: MainAxisSize.min,
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  Text(
                    controller.subject,
                    maxLines: 1,
                    overflow: TextOverflow.ellipsis,
                    style: const TextStyle(fontWeight: FontWeight.w800),
                  ),
                  Text(
                    controller.role,
                    maxLines: 1,
                    overflow: TextOverflow.ellipsis,
                    style: TextStyle(color: colors.textSecondary, fontSize: 11),
                  ),
                ],
              ),
            ),
          ],
        ],
      ),
    );
  }
}

class _Sidebar extends StatelessWidget {
  const _Sidebar({
    required this.current,
    required this.onNavigate,
  });

  final AppSection current;
  final ValueChanged<AppSection> onNavigate;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    return Container(
      decoration: BoxDecoration(
        color: colors.sidebar,
        border: Border(
            right: BorderSide(color: Colors.white.withValues(alpha: 0.08))),
      ),
      child: SafeArea(
        child: Padding(
          padding: const EdgeInsets.fromLTRB(18, 20, 18, 16),
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Row(
                children: [
                  Container(
                    width: 34,
                    height: 34,
                    decoration: BoxDecoration(
                      color: colors.primary,
                      shape: BoxShape.circle,
                      boxShadow: [
                        BoxShadow(
                          color: colors.primary.withValues(alpha: 0.28),
                          blurRadius: 18,
                          offset: const Offset(0, 8),
                        ),
                      ],
                    ),
                    child: const Icon(
                      LucideIcons.circlePlay,
                      color: Colors.white,
                      size: 18,
                    ),
                  ),
                  const SizedBox(width: 10),
                  const Expanded(
                    child: Text(
                      'StreamServer 控制台',
                      maxLines: 1,
                      overflow: TextOverflow.ellipsis,
                      style: TextStyle(
                        color: Colors.white,
                        fontSize: 16,
                        fontWeight: FontWeight.w800,
                      ),
                    ),
                  ),
                ],
              ),
              const SizedBox(height: 26),
              Expanded(
                child: ListView(
                  children: [
                    for (final group in _navGroups) ...[
                      _NavGroupTitle(group.label),
                      const SizedBox(height: 8),
                      for (final item in group.items)
                        _NavButton(
                          item: item,
                          selected: current == item.section,
                          onTap: () => onNavigate(item.section),
                        ),
                      const SizedBox(height: 18),
                    ],
                  ],
                ),
              ),
              _SidebarAccount(),
            ],
          ),
        ),
      ),
    );
  }
}

class _NavGroupTitle extends StatelessWidget {
  const _NavGroupTitle(this.label);

  final String label;

  @override
  Widget build(BuildContext context) {
    return Text(
      label,
      style: TextStyle(
        color: Colors.white.withValues(alpha: 0.45),
        fontSize: 12,
        fontWeight: FontWeight.w700,
      ),
    );
  }
}

class _NavButton extends StatelessWidget {
  const _NavButton({
    required this.item,
    required this.selected,
    required this.onTap,
  });

  final NavItem item;
  final bool selected;
  final VoidCallback onTap;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    return Padding(
      padding: const EdgeInsets.only(bottom: 6),
      child: Material(
        color: Colors.transparent,
        child: InkWell(
          borderRadius: BorderRadius.circular(8),
          onTap: onTap,
          child: Container(
            height: 40,
            padding: const EdgeInsets.symmetric(horizontal: 11),
            decoration: BoxDecoration(
              color: selected ? colors.primary : Colors.transparent,
              border: Border.all(
                color: selected
                    ? Colors.white.withValues(alpha: 0.08)
                    : Colors.transparent,
              ),
              borderRadius: BorderRadius.circular(8),
            ),
            child: Row(
              children: [
                Icon(
                  item.icon,
                  size: 18,
                  color: selected
                      ? Colors.white
                      : Colors.white.withValues(alpha: 0.78),
                ),
                const SizedBox(width: 11),
                Expanded(
                  child: Text(
                    item.label,
                    maxLines: 1,
                    overflow: TextOverflow.ellipsis,
                    style: TextStyle(
                      color: selected
                          ? Colors.white
                          : Colors.white.withValues(alpha: 0.86),
                      fontWeight: selected ? FontWeight.w800 : FontWeight.w600,
                      fontSize: 14,
                    ),
                  ),
                ),
                if (item.section == AppSection.tasks)
                  _TaskTotalBadge(selected: selected)
                else if (item.badge != null)
                  _NavBadge(label: item.badge!, selected: selected),
              ],
            ),
          ),
        ),
      ),
    );
  }
}

class _NavBadge extends StatelessWidget {
  const _NavBadge({required this.label, required this.selected});

  final String label;
  final bool selected;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    return Container(
      padding: const EdgeInsets.symmetric(horizontal: 7, vertical: 2),
      decoration: BoxDecoration(
        color: selected
            ? Colors.white.withValues(alpha: 0.18)
            : colors.primary.withValues(alpha: 0.85),
        borderRadius: BorderRadius.circular(999),
      ),
      child: Text(
        label,
        style: const TextStyle(
          color: Colors.white,
          fontSize: 11,
          fontWeight: FontWeight.w800,
        ),
      ),
    );
  }
}

class _TaskTotalBadge extends StatefulWidget {
  const _TaskTotalBadge({required this.selected});

  final bool selected;

  @override
  State<_TaskTotalBadge> createState() => _TaskTotalBadgeState();
}

class _TaskTotalBadgeState extends State<_TaskTotalBadge> {
  int? _total;
  int _loadedSeed = -1;

  @override
  void initState() {
    super.initState();
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (mounted) _load();
    });
  }

  @override
  void didChangeDependencies() {
    super.didChangeDependencies();
    final seed = AppScope.of(context).viewRefreshSeed;
    if (_loadedSeed >= 0 && _loadedSeed != seed) Future.microtask(_load);
  }

  Future<void> _load() async {
    final controller = AppScope.of(context);
    final seed = controller.viewRefreshSeed;
    try {
      final page = await controller.api(
        'GET',
        '/api/v1/tasks',
        query: {'page_size': 1},
      );
      final map = (page as Map).cast<String, Object?>();
      if (!mounted) return;
      setState(() {
        _total =
            (map['total'] as num?)?.toInt() ?? rowsFrom(map['items']).length;
        _loadedSeed = seed;
      });
    } catch (_) {
      if (!mounted) return;
      setState(() {
        _total = null;
        _loadedSeed = seed;
      });
    }
  }

  @override
  Widget build(BuildContext context) {
    final total = _total;
    if (total == null || total <= 0) return const SizedBox.shrink();
    return _NavBadge(
      label: total > 999 ? '999+' : '$total',
      selected: widget.selected,
    );
  }
}

class _SidebarAccount extends StatelessWidget {
  @override
  Widget build(BuildContext context) {
    final controller = AppScope.of(context);
    return Container(
      padding: const EdgeInsets.only(top: 16),
      decoration: BoxDecoration(
        border: Border(
          top: BorderSide(color: Colors.white.withValues(alpha: 0.08)),
        ),
      ),
      child: Row(
        children: [
          CircleAvatar(
            backgroundColor: Colors.white.withValues(alpha: 0.12),
            foregroundColor: Colors.white,
            child: Text(
              controller.subject.isEmpty
                  ? 'A'
                  : controller.subject.characters.first.toUpperCase(),
            ),
          ),
          const SizedBox(width: 10),
          Expanded(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text(
                  controller.subject,
                  maxLines: 1,
                  overflow: TextOverflow.ellipsis,
                  style: const TextStyle(
                    color: Colors.white,
                    fontWeight: FontWeight.w800,
                  ),
                ),
                Text(
                  controller.role,
                  maxLines: 1,
                  overflow: TextOverflow.ellipsis,
                  style: TextStyle(
                    color: Colors.white.withValues(alpha: 0.56),
                    fontSize: 12,
                  ),
                ),
              ],
            ),
          ),
          IconButton(
            tooltip: '退出',
            onPressed: controller.logout,
            icon: const Icon(LucideIcons.logOut, color: Colors.white, size: 18),
          ),
        ],
      ),
    );
  }
}

class _CompactTabs extends StatelessWidget {
  const _CompactTabs({required this.current, required this.onNavigate});

  final AppSection current;
  final ValueChanged<AppSection> onNavigate;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    return Container(
      height: 54,
      decoration: BoxDecoration(
        color: colors.surface,
        border: Border(bottom: BorderSide(color: colors.border)),
      ),
      child: ListView.separated(
        padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 9),
        scrollDirection: Axis.horizontal,
        itemCount: navItems.length,
        separatorBuilder: (_, __) => const SizedBox(width: 8),
        itemBuilder: (context, index) {
          final item = navItems[index];
          final selected = current == item.section;
          return ChoiceChip(
            selected: selected,
            label: Text(item.label),
            avatar: Icon(item.icon, size: 16),
            onSelected: (_) => onNavigate(item.section),
          );
        },
      ),
    );
  }
}

class _RightInspector extends StatefulWidget {
  const _RightInspector({required this.current, required this.controller});

  final AppSection current;
  final AppController controller;

  @override
  State<_RightInspector> createState() => _RightInspectorState();
}

class _RightInspectorState extends State<_RightInspector> {
  List<Map<String, Object?>> _nodes = const [];
  Object? _error;
  bool _loading = true;
  bool _refreshing = false;
  int _loadedSeed = -1;

  @override
  void initState() {
    super.initState();
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (mounted) _load();
    });
  }

  @override
  void didUpdateWidget(covariant _RightInspector oldWidget) {
    super.didUpdateWidget(oldWidget);
    if (oldWidget.controller.server?.baseUrl !=
            widget.controller.server?.baseUrl ||
        _loadedSeed != widget.controller.viewRefreshSeed) {
      Future.microtask(() => _load(silent: _nodes.isNotEmpty));
    }
  }

  Future<void> _load({bool silent = false}) async {
    if (!mounted) return;
    setState(() {
      if (silent) {
        _refreshing = true;
      } else {
        _loading = true;
      }
      _error = null;
    });
    try {
      final payload = await widget.controller.api('GET', '/api/v1/nodes');
      final nodes = rowsFrom((payload as Map)['value']);
      final selected = widget.controller.inspectorNodeId;
      if (nodes.isNotEmpty &&
          (selected == null ||
              !nodes.any((node) => '${node['id']}' == selected))) {
        widget.controller.selectInspectorNode('${nodes.first['id']}');
      }
      if (!mounted) return;
      setState(() {
        _nodes = nodes;
        _loadedSeed = widget.controller.viewRefreshSeed;
        _loading = false;
        _refreshing = false;
      });
    } catch (cause) {
      if (!mounted) return;
      setState(() {
        _error = cause;
        _loadedSeed = widget.controller.viewRefreshSeed;
        _loading = false;
        _refreshing = false;
      });
    }
  }

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    final selectedNode = _selectedNode;
    return Container(
      width: 330,
      margin: const EdgeInsets.fromLTRB(0, 22, 18, 22),
      decoration: BoxDecoration(
        color: colors.surface,
        border: Border.all(color: colors.border),
        borderRadius: BorderRadius.circular(12),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Padding(
            padding: const EdgeInsets.fromLTRB(16, 14, 12, 12),
            child: Row(
              children: [
                Expanded(
                  child: Text(
                    widget.current == AppSection.nodes ? '节点详情' : '运行摘要',
                    style: TextStyle(
                      color: colors.textPrimary,
                      fontWeight: FontWeight.w800,
                    ),
                  ),
                ),
                if (_refreshing)
                  Padding(
                    padding: const EdgeInsets.only(right: 8),
                    child: SizedBox.square(
                      dimension: 14,
                      child: CircularProgressIndicator(
                        strokeWidth: 2,
                        color: colors.primary,
                      ),
                    ),
                  ),
                IconButton(
                  tooltip: '隐藏运行摘要',
                  visualDensity: VisualDensity.compact,
                  onPressed: () => widget.controller.setInspectorVisible(false),
                  icon: Icon(LucideIcons.x,
                      size: 18, color: colors.textSecondary),
                ),
              ],
            ),
          ),
          Divider(height: 1, color: colors.border),
          Expanded(
            child: _loading
                ? Center(
                    child: CircularProgressIndicator(color: colors.primary),
                  )
                : _error != null
                    ? _InspectorError(error: _error!, onRetry: _load)
                    : _nodes.isEmpty
                        ? const Center(child: Text('暂无节点'))
                        : ListView(
                            padding: const EdgeInsets.all(16),
                            children: [
                              if (_nodes.length > 1) ...[
                                DropdownButtonFormField<String>(
                                  initialValue: '${selectedNode?['id']}',
                                  decoration: const InputDecoration(
                                    labelText: '节点',
                                    isDense: true,
                                  ),
                                  items: [
                                    for (final node in _nodes)
                                      DropdownMenuItem(
                                        value: '${node['id']}',
                                        child: Text(
                                          _nodeTitle(node),
                                          overflow: TextOverflow.ellipsis,
                                        ),
                                      ),
                                  ],
                                  onChanged:
                                      widget.controller.selectInspectorNode,
                                ),
                                const SizedBox(height: 16),
                              ],
                              if (selectedNode != null)
                                _NodeSummary(node: selectedNode),
                              const SizedBox(height: 18),
                              if (selectedNode != null)
                                _NodeMetricGrid(node: selectedNode),
                              const SizedBox(height: 20),
                              if (selectedNode != null)
                                _NodeServiceStatus(node: selectedNode),
                              const SizedBox(height: 20),
                              if (selectedNode != null)
                                _NodeHeartbeatInfo(node: selectedNode),
                              const SizedBox(height: 18),
                              if (widget.controller.server != null)
                                _InfoRow(
                                  label: 'Core',
                                  value: widget.controller.server!.baseUrl,
                                ),
                              if (widget.controller.selectedTaskId != null)
                                _InfoRow(
                                  label: '选中任务',
                                  value: widget.controller.selectedTaskId!,
                                ),
                            ],
                          ),
          ),
        ],
      ),
    );
  }

  Map<String, Object?>? get _selectedNode {
    if (_nodes.isEmpty) return null;
    final selected = widget.controller.inspectorNodeId;
    return _nodes.firstWhere(
      (node) => '${node['id']}' == selected,
      orElse: () => _nodes.first,
    );
  }

  String _nodeTitle(Map<String, Object?> node) {
    return textValue(node['node_name'] ?? node['hostname'] ?? node['id']);
  }
}

class _InspectorError extends StatelessWidget {
  const _InspectorError({required this.error, required this.onRetry});

  final Object error;
  final VoidCallback onRetry;

  @override
  Widget build(BuildContext context) {
    return Padding(
      padding: const EdgeInsets.all(16),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          const Text('节点摘要加载失败', style: TextStyle(fontWeight: FontWeight.w800)),
          const SizedBox(height: 8),
          Text(error.toString()),
          const SizedBox(height: 12),
          OutlinedButton.icon(
            onPressed: onRetry,
            icon: const Icon(LucideIcons.refreshCw, size: 16),
            label: const Text('重试'),
          ),
        ],
      ),
    );
  }
}

class _NodeSummary extends StatelessWidget {
  const _NodeSummary({required this.node});

  final Map<String, Object?> node;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    final healthy = node['healthy'] == true;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Row(
          children: [
            Container(
              width: 10,
              height: 10,
              decoration: BoxDecoration(
                color: healthy ? colors.success : colors.danger,
                shape: BoxShape.circle,
              ),
            ),
            const SizedBox(width: 8),
            Expanded(
              child: Text(
                textValue(node['node_name'] ?? node['hostname'] ?? node['id']),
                style: TextStyle(
                  color: colors.textPrimary,
                  fontSize: 18,
                  fontWeight: FontWeight.w800,
                ),
              ),
            ),
            StatusBadge(status: healthy ? 'healthy' : 'unhealthy'),
          ],
        ),
        const SizedBox(height: 10),
        _OptionalInfoRow(label: '节点 ID', value: node['id']),
        _OptionalInfoRow(label: '主机名', value: node['hostname']),
        _OptionalInfoRow(label: '版本', value: node['agent_version']),
        _OptionalInfoRow(label: '标签', value: node['labels']),
        _OptionalInfoRow(label: '网卡', value: node['interfaces']),
      ],
    );
  }
}

class _NodeMetricGrid extends StatelessWidget {
  const _NodeMetricGrid({required this.node});

  final Map<String, Object?> node;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    final metrics = [
      _MetricSpec('CPU', _percent(node['cpu_percent']), colors.primary),
      _MetricSpec('内存', _percent(node['mem_percent']), colors.purple),
      _MetricSpec('磁盘', _percent(node['disk_percent']), colors.orange),
      _MetricSpec('运行任务', _plain(node['running_tasks']), colors.success),
      _MetricSpec('启动中', _plain(node['starting_tasks']), colors.primary),
      _MetricSpec('停止中', _plain(node['stopping_tasks']), colors.orange),
    ].where((item) => item.value != null).toList();
    if (metrics.isEmpty) return const SizedBox.shrink();
    return Wrap(
      spacing: 10,
      runSpacing: 10,
      children: [
        for (final metric in metrics)
          _MiniMetric(
            label: metric.label,
            value: metric.value!,
            tone: metric.tone,
          ),
      ],
    );
  }
}

class _MetricSpec {
  const _MetricSpec(this.label, this.value, this.tone);

  final String label;
  final String? value;
  final Color tone;
}

class _NodeServiceStatus extends StatelessWidget {
  const _NodeServiceStatus({required this.node});

  final Map<String, Object?> node;

  @override
  Widget build(BuildContext context) {
    final rows = [
      _BoolSpec('控制连接', node['control_connected']),
      _BoolSpec('媒体服务', node['media_alive']),
      _BoolSpec('ZLM', node['zlm_alive']),
      _BoolSpec('FFmpeg', node['ffmpeg_alive']),
    ].where((item) => item.value is bool).toList();
    if (rows.isEmpty) return const SizedBox.shrink();
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        const _SectionTitle('服务状态'),
        const SizedBox(height: 10),
        for (final row in rows) _BooleanStatusRow(row.label, row.value == true),
      ],
    );
  }
}

class _BoolSpec {
  const _BoolSpec(this.label, this.value);

  final String label;
  final Object? value;
}

class _NodeHeartbeatInfo extends StatelessWidget {
  const _NodeHeartbeatInfo({required this.node});

  final Map<String, Object?> node;

  @override
  Widget build(BuildContext context) {
    final rows = [
      MapEntry('最后心跳', node['last_seen_at']),
      MapEntry('控制心跳', node['control_last_seen_at']),
      MapEntry('媒体心跳', node['media_last_seen_at']),
    ].where((entry) => _hasValue(entry.value)).toList();
    if (rows.isEmpty) return const SizedBox.shrink();
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        const _SectionTitle('心跳信息'),
        const SizedBox(height: 10),
        for (final row in rows)
          _InfoRow(label: row.key, value: _displayValue(row.value)),
      ],
    );
  }
}

class _MiniMetric extends StatelessWidget {
  const _MiniMetric({
    required this.label,
    required this.value,
    required this.tone,
  });

  final String label;
  final String value;
  final Color tone;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    return Container(
      width: 136,
      padding: const EdgeInsets.all(12),
      decoration: BoxDecoration(
        color: colors.surfaceAlt,
        border: Border.all(color: colors.border),
        borderRadius: BorderRadius.circular(8),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text(label,
              style: TextStyle(color: colors.textSecondary, fontSize: 12)),
          const SizedBox(height: 8),
          Row(
            children: [
              Expanded(
                child: Text(
                  value,
                  style: TextStyle(
                    color: colors.textPrimary,
                    fontSize: 19,
                    fontWeight: FontWeight.w800,
                  ),
                ),
              ),
              Icon(LucideIcons.activity, size: 18, color: tone),
            ],
          ),
        ],
      ),
    );
  }
}

class _SectionTitle extends StatelessWidget {
  const _SectionTitle(this.text);

  final String text;

  @override
  Widget build(BuildContext context) {
    return Text(
      text,
      style: TextStyle(
        color: context.streamColors.textPrimary,
        fontWeight: FontWeight.w800,
      ),
    );
  }
}

class _BooleanStatusRow extends StatelessWidget {
  const _BooleanStatusRow(this.name, this.ok);

  final String name;
  final bool ok;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    return Padding(
      padding: const EdgeInsets.only(bottom: 9),
      child: Row(
        children: [
          Icon(LucideIcons.circle,
              size: 10, color: ok ? colors.success : colors.danger),
          const SizedBox(width: 8),
          Expanded(child: Text(name)),
          StatusBadge(status: ok ? 'running' : 'offline'),
        ],
      ),
    );
  }
}

class _InfoRow extends StatelessWidget {
  const _InfoRow({required this.label, required this.value});

  final String label;
  final String value;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    return Padding(
      padding: const EdgeInsets.only(bottom: 8),
      child: Row(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          SizedBox(
            width: 74,
            child: Text(
              label,
              style: TextStyle(color: colors.textSecondary, fontSize: 13),
            ),
          ),
          Expanded(
            child: SelectableText(
              value,
              style: TextStyle(color: colors.textPrimary, fontSize: 13),
            ),
          ),
        ],
      ),
    );
  }
}

class _OptionalInfoRow extends StatelessWidget {
  const _OptionalInfoRow({required this.label, required this.value});

  final String label;
  final Object? value;

  @override
  Widget build(BuildContext context) {
    if (!_hasValue(value)) return const SizedBox.shrink();
    return _InfoRow(label: label, value: _displayValue(value));
  }
}

String? _percent(Object? value) {
  if (!_hasValue(value)) return null;
  final number = value is num ? value : num.tryParse('$value');
  if (number == null) return textValue(value);
  return '${number.toStringAsFixed(number % 1 == 0 ? 0 : 1)}%';
}

String? _plain(Object? value) {
  if (!_hasValue(value)) return null;
  return _displayValue(value);
}

bool _hasValue(Object? value) {
  if (value == null) return false;
  if (value is String) return value.trim().isNotEmpty;
  if (value is Iterable) return value.isNotEmpty;
  if (value is Map) return value.isNotEmpty;
  return true;
}

String _displayValue(Object? value) {
  if (value is Iterable) {
    return value
        .map((item) => '$item')
        .where((item) => item.isNotEmpty)
        .join(', ');
  }
  if (value is Map) return prettyJson(value);
  return textValue(value);
}

NavItem navItemFor(AppSection section) {
  if (section == AppSection.taskDetail) {
    return const NavItem(
      label: '任务详情',
      note: '任务事件、日志、产物和播放入口',
      section: AppSection.taskDetail,
      icon: LucideIcons.panelRight,
    );
  }
  return navItems.firstWhere((item) => item.section == section);
}

class NavGroup {
  const NavGroup(this.label, this.items);

  final String label;
  final List<NavItem> items;
}

class NavItem {
  const NavItem({
    required this.label,
    required this.note,
    required this.section,
    required this.icon,
    this.badge,
  });

  final String label;
  final String note;
  final AppSection section;
  final IconData icon;
  final String? badge;
}

const navItems = [
  NavItem(
    label: '系统总览',
    note: '实时监控系统运行状态与核心指标',
    section: AppSection.overview,
    icon: LucideIcons.house,
  ),
  NavItem(
    label: '任务中心',
    note: '任务生命周期、筛选、批量运维',
    section: AppSection.tasks,
    icon: LucideIcons.listChecks,
  ),
  NavItem(
    label: '录制中心',
    note: '录像检索、预览与文件定位',
    section: AppSection.records,
    icon: LucideIcons.video,
  ),
  NavItem(
    label: '流中心',
    note: '在线流、播放地址与关流操作',
    section: AppSection.streams,
    icon: LucideIcons.radio,
  ),
  NavItem(
    label: '组播中心',
    note: 'RTP/组播相关流视图',
    section: AppSection.multicast,
    icon: LucideIcons.network,
  ),
  NavItem(
    label: '文件产物',
    note: '转码、桥接和快录文件资产',
    section: AppSection.artifacts,
    icon: LucideIcons.file,
  ),
  NavItem(
    label: '媒资上传',
    note: '上传队列、历史和关联播放',
    section: AppSection.uploads,
    icon: LucideIcons.upload,
  ),
  NavItem(
    label: '节点中心',
    note: '节点健康、容量、心跳与能力',
    section: AppSection.nodes,
    icon: LucideIcons.server,
  ),
  NavItem(
    label: '安全设置',
    note: '认证、改密和机器白名单',
    section: AppSection.security,
    icon: LucideIcons.shield,
  ),
  NavItem(
    label: '调试台',
    note: 'ZLM、Hook、播放器和诊断工具',
    section: AppSection.debug,
    icon: LucideIcons.bug,
  ),
  NavItem(
    label: '新建任务',
    note: '创建 StreamServer 任务规格',
    section: AppSection.taskCreate,
    icon: LucideIcons.plus,
  ),
];

final _navGroups = [
  NavGroup('运行监控', [
    navItems[0],
  ]),
  NavGroup('任务管理', [
    navItems[1],
    navItems[10],
  ]),
  NavGroup('资源管理', [
    navItems[3],
    navItems[4],
    navItems[2],
    navItems[5],
    navItems[6],
    navItems[7],
  ]),
  NavGroup('系统管理', [
    navItems[8],
    navItems[9],
  ]),
];
