import 'package:flutter/material.dart';

import '../state.dart';

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
