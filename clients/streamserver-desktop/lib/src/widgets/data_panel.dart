import 'package:flutter/material.dart';
import 'package:lucide_icons_flutter/lucide_icons.dart';

import '../core/theme/stream_theme.dart';
import '../state.dart';
import '../utils.dart';

class PageHeader extends StatelessWidget {
  const PageHeader({
    required this.title,
    required this.description,
    this.actions,
    super.key,
  });

  final String title;
  final String description;
  final Widget? actions;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    return LayoutBuilder(
      builder: (context, constraints) {
        final compact = constraints.maxWidth < 640;
        final titleBlock = Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text(
              title,
              style: TextStyle(
                color: colors.textPrimary,
                fontSize: 22,
                fontWeight: FontWeight.w800,
                height: 1.12,
              ),
            ),
            const SizedBox(height: 8),
            Text(
              description,
              style: TextStyle(color: colors.textSecondary, fontSize: 13),
            ),
          ],
        );
        return Padding(
          padding: const EdgeInsets.only(bottom: 18),
          child: compact
              ? Column(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    titleBlock,
                    if (actions != null) ...[
                      const SizedBox(height: 12),
                      actions!,
                    ],
                  ],
                )
              : Row(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Expanded(child: titleBlock),
                    if (actions != null) actions!,
                  ],
                ),
        );
      },
    );
  }
}

class Surface extends StatelessWidget {
  const Surface({
    required this.child,
    this.padding,
    this.flat = false,
    super.key,
  });

  final Widget child;
  final EdgeInsetsGeometry? padding;
  final bool flat;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    return LayoutBuilder(
      builder: (context, constraints) {
        final padding = constraints.maxWidth < 520 ? 12.0 : 18.0;
        return DecoratedBox(
          decoration: BoxDecoration(
            color: colors.surface,
            border: Border.all(color: colors.border),
            borderRadius: BorderRadius.circular(12),
            boxShadow: flat
                ? null
                : [
                    BoxShadow(
                      color: Colors.black
                          .withValues(alpha: context.isDarkMode ? 0.18 : 0.035),
                      blurRadius: 18,
                      offset: const Offset(0, 10),
                    ),
                  ],
          ),
          child: Padding(
            padding: this.padding ?? EdgeInsets.all(padding),
            child: SizedBox(width: double.infinity, child: child),
          ),
        );
      },
    );
  }
}

class AsyncDataPanel extends StatefulWidget {
  const AsyncDataPanel({
    required this.loader,
    required this.builder,
    super.key,
  });

  final Future<Object?> Function(AppController controller) loader;
  final Widget Function(BuildContext context, Object? data) builder;

  @override
  State<AsyncDataPanel> createState() => _AsyncDataPanelState();
}

class _AsyncDataPanelState extends State<AsyncDataPanel> {
  Object? data;
  Object? error;
  bool loading = true;
  bool refreshing = false;
  int loadedRefreshSeed = -1;

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
    if (loadedRefreshSeed >= 0 && loadedRefreshSeed != seed && !refreshing) {
      Future.microtask(() => _load(silent: true));
    }
  }

  Future<void> _load({bool silent = false}) async {
    final seed = AppScope.of(context).viewRefreshSeed;
    if (mounted) {
      setState(() {
        if (silent && data != null) {
          refreshing = true;
        } else {
          loading = true;
        }
        error = null;
      });
    }
    try {
      data = await widget.loader(AppScope.of(context));
      loadedRefreshSeed = seed;
    } catch (cause) {
      error = cause;
    } finally {
      if (mounted) {
        setState(() {
          loading = false;
          refreshing = false;
        });
      }
    }
  }

  @override
  Widget build(BuildContext context) {
    if (loading) {
      return Surface(
        child: SizedBox(
          height: 160,
          child: Center(
            child: CircularProgressIndicator(
              color: context.streamColors.primary,
            ),
          ),
        ),
      );
    }
    if (error != null) {
      return Surface(
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            const Text('加载失败', style: TextStyle(fontWeight: FontWeight.w700)),
            const SizedBox(height: 8),
            Text(error.toString()),
            const SizedBox(height: 12),
            OutlinedButton.icon(
              onPressed: _load,
              icon: const Icon(LucideIcons.refreshCw, size: 17),
              label: const Text('重试'),
            ),
          ],
        ),
      );
    }
    return Stack(
      children: [
        widget.builder(context, data),
        if (refreshing)
          Positioned(
            top: 0,
            right: 0,
            child: Padding(
              padding: const EdgeInsets.all(8),
              child: SizedBox.square(
                dimension: 16,
                child: CircularProgressIndicator(
                  strokeWidth: 2,
                  color: context.streamColors.primary,
                ),
              ),
            ),
          ),
      ],
    );
  }
}

class KeyValueGrid extends StatelessWidget {
  const KeyValueGrid({required this.items, super.key});

  final Map<String, Object?> items;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    return Wrap(
      spacing: 12,
      runSpacing: 12,
      children: items.entries.map((entry) {
        return SizedBox(
          width: 218,
          child: Surface(
            padding: const EdgeInsets.all(16),
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text(entry.key,
                    style:
                        TextStyle(color: colors.textSecondary, fontSize: 13)),
                const SizedBox(height: 8),
                if (_isStatusKey(entry.key))
                  StatusBadge(status: entry.value, large: true)
                else
                  Text(
                    '${entry.value ?? '—'}',
                    style: TextStyle(
                      color: colors.textPrimary,
                      fontSize: 25,
                      fontWeight: FontWeight.w800,
                      height: 1.1,
                    ),
                    overflow: TextOverflow.ellipsis,
                  ),
              ],
            ),
          ),
        );
      }).toList(),
    );
  }
}

class StatusBadge extends StatelessWidget {
  const StatusBadge({
    required this.status,
    this.large = false,
    super.key,
  });

  final Object? status;
  final bool large;

  @override
  Widget build(BuildContext context) {
    final rawText = textValue(status);
    final text = _statusLabel(rawText);
    final tone = _statusTone(rawText);
    return DecoratedBox(
      decoration: BoxDecoration(
        color: tone.background,
        border: Border.all(color: tone.border),
        borderRadius: BorderRadius.circular(999),
      ),
      child: Padding(
        padding: EdgeInsets.symmetric(
          horizontal: large ? 12 : 10,
          vertical: large ? 7 : 5,
        ),
        child: Row(
          mainAxisSize: MainAxisSize.min,
          children: [
            Container(
              width: large ? 9 : 7,
              height: large ? 9 : 7,
              decoration: BoxDecoration(
                color: tone.foreground,
                shape: BoxShape.circle,
              ),
            ),
            SizedBox(width: large ? 8 : 6),
            Text(
              text,
              style: TextStyle(
                color: tone.foreground,
                fontSize: large ? 15 : 12,
                fontWeight: FontWeight.w800,
              ),
            ),
          ],
        ),
      ),
    );
  }
}

class WrappedTextCell extends StatelessWidget {
  const WrappedTextCell({
    required this.value,
    required this.maxWidth,
    this.selectable = false,
    this.fontWeight,
    super.key,
  });

  final Object? value;
  final double maxWidth;
  final bool selectable;
  final FontWeight? fontWeight;

  @override
  Widget build(BuildContext context) {
    final text = textValue(value);
    final style = TextStyle(
      color: context.streamColors.textPrimary,
      fontWeight: fontWeight,
      fontSize: 13,
      height: 1.35,
    );
    return ConstrainedBox(
      constraints: BoxConstraints(maxWidth: maxWidth),
      child: selectable
          ? SelectableText(text, style: style)
          : Text(
              text,
              softWrap: true,
              overflow: TextOverflow.visible,
              style: style,
            ),
    );
  }
}

bool _isStatusKey(String key) =>
    key.toLowerCase().contains('status') || key.contains('状态');

_StatusTone _statusTone(String value) {
  switch (value.trim().toUpperCase()) {
    case 'RUNNING':
      return const _StatusTone(
          Color(0xff05603a), Color(0xffdcfae6), Color(0xff75e0a7));
    case 'SUCCEEDED':
    case 'COMPLETED':
    case 'SUCCESS':
    case 'ACTIVE':
    case 'HEALTHY':
    case 'CONNECTED':
    case 'ALIVE':
    case 'TRUE':
      return const _StatusTone(
          Color(0xff027a48), Color(0xffecfdf3), Color(0xffabefc6));
    case 'FAILED':
    case 'ERROR':
    case 'UNHEALTHY':
    case 'DISCONNECTED':
    case 'DEAD':
    case 'OFFLINE':
    case 'FALSE':
      return const _StatusTone(
          Color(0xffb42318), Color(0xfffff1f3), Color(0xfffda29b));
    case 'LOST':
      return const _StatusTone(
          Color(0xffb54708), Color(0xfffff6ed), Color(0xfffdba74));
    case 'CANCELED':
    case 'CANCELLED':
    case 'DELETED':
      return const _StatusTone(
          Color(0xff475467), Color(0xfff2f4f7), Color(0xffd0d5dd));
    case 'QUEUED':
    case 'CREATED':
      return const _StatusTone(
          Color(0xff175cd3), Color(0xffeff8ff), Color(0xff84caff));
    case 'VALIDATING':
    case 'STARTING':
      return const _StatusTone(
          Color(0xff026aa2), Color(0xfff0f9ff), Color(0xff7cd4fd));
    case 'STOPPING':
      return const _StatusTone(
          Color(0xffc4320a), Color(0xfffff6ed), Color(0xfffd853a));
    case 'STOPPED':
    case 'PAUSED':
    case 'IDLE':
      return const _StatusTone(
          Color(0xff475467), Color(0xfff2f4f7), Color(0xffd0d5dd));
    case 'PENDING':
    case 'PROCESSING':
    case 'UPLOADING':
      return const _StatusTone(
          Color(0xff175cd3), Color(0xffeff8ff), Color(0xff84caff));
    case 'RECOVERING':
      return const _StatusTone(
          Color(0xff6941c6), Color(0xfff4f3ff), Color(0xffbdb4fe));
    default:
      return const _StatusTone(
          Color(0xff344054), Color(0xfff8fafc), Color(0xffcbd5e1));
  }
}

String _statusLabel(String value) {
  switch (value.trim().toUpperCase()) {
    case 'RUNNING':
      return '运行中';
    case 'SUCCEEDED':
    case 'SUCCESS':
      return '成功';
    case 'COMPLETED':
      return '已完成';
    case 'FAILED':
      return '失败';
    case 'ERROR':
      return '错误';
    case 'LOST':
      return '失联';
    case 'CANCELED':
    case 'CANCELLED':
      return '已取消';
    case 'DELETED':
      return '已删除';
    case 'QUEUED':
      return '排队中';
    case 'CREATED':
      return '已创建';
    case 'VALIDATING':
      return '校验中';
    case 'STARTING':
      return '启动中';
    case 'STOPPING':
      return '停止中';
    case 'STOPPED':
      return '已停止';
    case 'PAUSED':
      return '已暂停';
    case 'IDLE':
      return '空闲';
    case 'PENDING':
      return '等待中';
    case 'PROCESSING':
      return '处理中';
    case 'UPLOADING':
      return '上传中';
    case 'RECOVERING':
      return '恢复中';
    case 'HEALTHY':
      return '健康';
    case 'UNHEALTHY':
      return '异常';
    case 'CONNECTED':
      return '已连接';
    case 'DISCONNECTED':
      return '未连接';
    case 'ALIVE':
      return '在线';
    case 'DEAD':
      return '离线';
    case 'ACTIVE':
      return '活跃';
    case 'TRUE':
      return '是';
    case 'FALSE':
      return '否';
    case 'RUNNING_TASKS':
      return '运行任务';
    case 'OFFLINE':
      return '离线';
    default:
      return value;
  }
}

class _StatusTone {
  const _StatusTone(this.foreground, this.background, this.border);

  final Color foreground;
  final Color background;
  final Color border;
}
