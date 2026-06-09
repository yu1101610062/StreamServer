import 'package:flutter/material.dart';

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
    return LayoutBuilder(
      builder: (context, constraints) {
        final compact = constraints.maxWidth < 640;
        final titleBlock = Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text(title, style: Theme.of(context).textTheme.headlineSmall),
            const SizedBox(height: 8),
            Text(description, style: const TextStyle(color: Color(0xff5b6477))),
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
  const Surface({required this.child, super.key});

  final Widget child;

  @override
  Widget build(BuildContext context) {
    return LayoutBuilder(
      builder: (context, constraints) {
        final padding = constraints.maxWidth < 520 ? 12.0 : 18.0;
        return Card(
          elevation: 0,
          color: Colors.white,
          child: Padding(
            padding: EdgeInsets.all(padding),
            child: child,
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

  @override
  void initState() {
    super.initState();
    Future.microtask(_load);
  }

  Future<void> _load() async {
    setState(() {
      loading = true;
      error = null;
    });
    try {
      data = await widget.loader(AppScope.of(context));
    } catch (cause) {
      error = cause;
    } finally {
      if (mounted) {
        setState(() {
          loading = false;
        });
      }
    }
  }

  @override
  Widget build(BuildContext context) {
    if (loading) {
      return const Surface(
        child: SizedBox(
            height: 160, child: Center(child: CircularProgressIndicator())),
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
              icon: const Icon(Icons.refresh),
              label: const Text('重试'),
            ),
          ],
        ),
      );
    }
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Align(
          alignment: Alignment.centerRight,
          child: IconButton(
            tooltip: '刷新',
            onPressed: _load,
            icon: const Icon(Icons.refresh),
          ),
        ),
        widget.builder(context, data),
      ],
    );
  }
}

class KeyValueGrid extends StatelessWidget {
  const KeyValueGrid({required this.items, super.key});

  final Map<String, Object?> items;

  @override
  Widget build(BuildContext context) {
    return Wrap(
      spacing: 12,
      runSpacing: 12,
      children: items.entries.map((entry) {
        return SizedBox(
          width: 220,
          child: Surface(
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text(entry.key,
                    style: const TextStyle(color: Color(0xff5b6477))),
                const SizedBox(height: 8),
                if (_isStatusKey(entry.key))
                  StatusBadge(status: entry.value, large: true)
                else
                  Text(
                    '${entry.value ?? '—'}',
                    style: const TextStyle(
                        fontSize: 24, fontWeight: FontWeight.w700),
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
    final text = textValue(status);
    final tone = _statusTone(text);
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
    final style = TextStyle(fontWeight: fontWeight);
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
    case 'RECOVERING':
      return const _StatusTone(
          Color(0xff6941c6), Color(0xfff4f3ff), Color(0xffbdb4fe));
    default:
      return const _StatusTone(
          Color(0xff344054), Color(0xfff8fafc), Color(0xffcbd5e1));
  }
}

class _StatusTone {
  const _StatusTone(this.foreground, this.background, this.border);

  final Color foreground;
  final Color background;
  final Color border;
}
