import 'package:flutter/material.dart';

import '../state.dart';
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
        final compact = constraints.maxWidth < 900;
        return Scaffold(
          body: compact
              ? _CompactShell(
                  controller: controller,
                  current: current,
                  onNavigate: onNavigate,
                  child: child,
                )
              : _WideShell(
                  controller: controller,
                  current: current,
                  onNavigate: onNavigate,
                  child: child,
                ),
        );
      },
    );
  }
}

class _WideShell extends StatelessWidget {
  const _WideShell({
    required this.controller,
    required this.current,
    required this.onNavigate,
    required this.child,
  });

  final AppController controller;
  final AppSection current;
  final ValueChanged<AppSection> onNavigate;
  final Widget child;

  @override
  Widget build(BuildContext context) {
    return LayoutBuilder(
      builder: (context, constraints) {
        final sideWidth = constraints.maxWidth < 1120 ? 232.0 : 272.0;
        return Row(
          children: [
            SizedBox(
              width: sideWidth,
              child: _SideNavigation(
                current: current,
                onNavigate: onNavigate,
                compact: sideWidth < 250,
              ),
            ),
            Expanded(
              child: _ContentColumn(
                controller: controller,
                onNavigate: onNavigate,
                padding: const EdgeInsets.all(24),
                child: child,
              ),
            ),
          ],
        );
      },
    );
  }
}

class _CompactShell extends StatelessWidget {
  const _CompactShell({
    required this.controller,
    required this.current,
    required this.onNavigate,
    required this.child,
  });

  final AppController controller;
  final AppSection current;
  final ValueChanged<AppSection> onNavigate;
  final Widget child;

  @override
  Widget build(BuildContext context) {
    return Column(
      children: [
        _TopBar(controller: controller, onNavigate: onNavigate),
        _CompactNavigation(current: current, onNavigate: onNavigate),
        Expanded(
          child: _ContentColumn(
            controller: controller,
            onNavigate: onNavigate,
            padding: const EdgeInsets.all(12),
            child: child,
          ),
        ),
      ],
    );
  }
}

class _ContentColumn extends StatelessWidget {
  const _ContentColumn({
    required this.controller,
    required this.onNavigate,
    required this.padding,
    required this.child,
  });

  final AppController controller;
  final ValueChanged<AppSection> onNavigate;
  final EdgeInsets padding;
  final Widget child;

  @override
  Widget build(BuildContext context) {
    return Column(
      children: [
        if (padding.left > 12)
          _TopBar(controller: controller, onNavigate: onNavigate),
        if (controller.activeMediaUrl != null)
          EmbeddedPlayerPanel(
            url: controller.activeMediaUrl!,
            title: controller.activeMediaTitle,
          ),
        Expanded(
          child: SingleChildScrollView(
            padding: padding,
            child: child,
          ),
        ),
      ],
    );
  }
}

class _TopBar extends StatelessWidget {
  const _TopBar({
    required this.controller,
    required this.onNavigate,
  });

  final AppController controller;
  final ValueChanged<AppSection> onNavigate;

  @override
  Widget build(BuildContext context) {
    return SafeArea(
      bottom: false,
      child: Container(
        width: double.infinity,
        padding: const EdgeInsets.symmetric(horizontal: 16, vertical: 12),
        decoration: const BoxDecoration(
          color: Colors.white,
          border: Border(bottom: BorderSide(color: Color(0xffe4e8f0))),
        ),
        child: Wrap(
          spacing: 12,
          runSpacing: 10,
          crossAxisAlignment: WrapCrossAlignment.center,
          children: [
            Container(
              padding: const EdgeInsets.symmetric(horizontal: 10, vertical: 6),
              decoration: BoxDecoration(
                color: const Color(0xffe8f0ff),
                borderRadius: BorderRadius.circular(999),
              ),
              child: Text(
                controller.environment,
                style: const TextStyle(
                  color: Color(0xff1463ff),
                  fontWeight: FontWeight.w700,
                ),
              ),
            ),
            ConstrainedBox(
              constraints: const BoxConstraints(maxWidth: 260),
              child: Text(
                '${controller.subject} · ${controller.role}',
                maxLines: 1,
                overflow: TextOverflow.ellipsis,
              ),
            ),
            FilledButton.icon(
              onPressed: () => onNavigate(AppSection.taskCreate),
              icon: const Icon(Icons.add),
              label: const Text('新建任务'),
            ),
            OutlinedButton.icon(
              onPressed: controller.logout,
              icon: const Icon(Icons.logout),
              label: const Text('退出'),
            ),
          ],
        ),
      ),
    );
  }
}

class _SideNavigation extends StatelessWidget {
  const _SideNavigation({
    required this.current,
    required this.onNavigate,
    required this.compact,
  });

  final AppSection current;
  final ValueChanged<AppSection> onNavigate;
  final bool compact;

  @override
  Widget build(BuildContext context) {
    return ColoredBox(
      color: const Color(0xff0b1526),
      child: SafeArea(
        child: Padding(
          padding: EdgeInsets.all(compact ? 14 : 20),
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              const Text(
                'STREAMSERVER',
                style: TextStyle(
                  color: Color(0xff9fb3d9),
                  fontSize: 12,
                  fontWeight: FontWeight.w700,
                  letterSpacing: 1.4,
                ),
              ),
              const SizedBox(height: 4),
              Text(
                '桌面控制台',
                style: TextStyle(
                  color: Colors.white,
                  fontSize: compact ? 18 : 22,
                  fontWeight: FontWeight.w700,
                ),
              ),
              const SizedBox(height: 24),
              Expanded(
                child: ListView(
                  children: _navItems
                      .map((item) => _NavButton(
                            item.label,
                            item.note,
                            item.section,
                            current,
                            onNavigate,
                            compact: compact,
                          ))
                      .toList(),
                ),
              ),
            ],
          ),
        ),
      ),
    );
  }
}

class _CompactNavigation extends StatelessWidget {
  const _CompactNavigation({
    required this.current,
    required this.onNavigate,
  });

  final AppSection current;
  final ValueChanged<AppSection> onNavigate;

  @override
  Widget build(BuildContext context) {
    return DecoratedBox(
      decoration: const BoxDecoration(
        color: Colors.white,
        border: Border(bottom: BorderSide(color: Color(0xffe4e8f0))),
      ),
      child: SingleChildScrollView(
        scrollDirection: Axis.horizontal,
        padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 10),
        child: Row(
          children: _navItems.map((item) {
            final selected = current == item.section;
            return Padding(
              padding: const EdgeInsets.only(right: 8),
              child: ChoiceChip(
                selected: selected,
                label: Text(item.label),
                onSelected: (_) => onNavigate(item.section),
              ),
            );
          }).toList(),
        ),
      ),
    );
  }
}

class _NavButton extends StatelessWidget {
  const _NavButton(
      this.label, this.note, this.section, this.current, this.onNavigate,
      {required this.compact});

  final String label;
  final String note;
  final AppSection section;
  final AppSection current;
  final ValueChanged<AppSection> onNavigate;
  final bool compact;

  @override
  Widget build(BuildContext context) {
    final selected = current == section;
    return Padding(
      padding: const EdgeInsets.only(bottom: 8),
      child: TextButton(
        style: TextButton.styleFrom(
          alignment: Alignment.centerLeft,
          padding: const EdgeInsets.all(14),
          foregroundColor: Colors.white,
          backgroundColor: selected
              ? Colors.white.withValues(alpha: 0.1)
              : Colors.transparent,
          shape: RoundedRectangleBorder(
            side: BorderSide(
              color: selected
                  ? Colors.white.withValues(alpha: 0.18)
                  : Colors.transparent,
            ),
            borderRadius: BorderRadius.circular(8),
          ),
        ),
        onPressed: () => onNavigate(section),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text(label, style: const TextStyle(fontWeight: FontWeight.w700)),
            if (!compact) ...[
              const SizedBox(height: 4),
              Text(
                note,
                style: const TextStyle(color: Color(0xffb6c4dd), fontSize: 12),
              ),
            ],
          ],
        ),
      ),
    );
  }
}

class _NavItem {
  const _NavItem(this.label, this.note, this.section);

  final String label;
  final String note;
  final AppSection section;
}

const _navItems = [
  _NavItem('系统总览', '任务、流、录像与节点概况', AppSection.overview),
  _NavItem('任务中心', '创建、启停、重试和克隆', AppSection.tasks),
  _NavItem('流中心', '在线流与播放地址', AppSection.streams),
  _NavItem('组播中心', '组播任务视图', AppSection.multicast),
  _NavItem('录像中心', '录像检索与播放', AppSection.records),
  _NavItem('文件产物', '转码与桥接输出', AppSection.artifacts),
  _NavItem('媒资上传', '本地文件上传到节点', AppSection.uploads),
  _NavItem('节点中心', '健康、容量和心跳', AppSection.nodes),
  _NavItem('安全设置', '密码与机器白名单', AppSection.security),
  _NavItem('调试台', 'ZLM 与 Hook 调试', AppSection.debug),
];
