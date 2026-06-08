import 'dart:math' as math;

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

Map<String, Object?> cleanQuery(Map<String, Object?> query) {
  final clean = <String, Object?>{};
  for (final entry in query.entries) {
    final value = entry.value;
    if (value == null) continue;
    if (value is String && value.trim().isEmpty) continue;
    clean[entry.key] = value;
  }
  return clean;
}

void showResult(BuildContext context, String message) {
  ScaffoldMessenger.of(context).showSnackBar(SnackBar(content: Text(message)));
}

Future<bool> confirmAction(
  BuildContext context, {
  required String title,
  required String message,
  String confirmLabel = '确认',
  bool destructive = false,
}) async {
  final result = await showDialog<bool>(
    context: context,
    builder: (context) => AlertDialog(
      title: Text(title),
      content: Text(message),
      actions: [
        TextButton(
          onPressed: () => Navigator.of(context).pop(false),
          child: const Text('取消'),
        ),
        FilledButton(
          style: destructive
              ? FilledButton.styleFrom(backgroundColor: Colors.red.shade700)
              : null,
          onPressed: () => Navigator.of(context).pop(true),
          child: Text(confirmLabel),
        ),
      ],
    ),
  );
  return result ?? false;
}

Future<void> copyText(BuildContext context, String value) async {
  await Clipboard.setData(ClipboardData(text: value));
  if (context.mounted) {
    showResult(context, '已复制');
  }
}

class FilterBar extends StatelessWidget {
  const FilterBar({
    required this.children,
    required this.onApply,
    this.onReset,
    super.key,
  });

  final List<Widget> children;
  final VoidCallback onApply;
  final VoidCallback? onReset;

  @override
  Widget build(BuildContext context) {
    return Wrap(
      spacing: 12,
      runSpacing: 12,
      crossAxisAlignment: WrapCrossAlignment.center,
      children: [
        ...children,
        FilledButton.icon(
          onPressed: onApply,
          icon: const Icon(Icons.filter_alt),
          label: const Text('应用筛选'),
        ),
        if (onReset != null)
          OutlinedButton.icon(
            onPressed: onReset,
            icon: const Icon(Icons.clear),
            label: const Text('重置'),
          ),
      ],
    );
  }
}

class SmallTextField extends StatelessWidget {
  const SmallTextField({
    required this.controller,
    required this.label,
    this.width = 220,
    this.onSubmitted,
    this.obscureText = false,
    super.key,
  });

  final TextEditingController controller;
  final String label;
  final double width;
  final ValueChanged<String>? onSubmitted;
  final bool obscureText;

  @override
  Widget build(BuildContext context) {
    return LayoutBuilder(
      builder: (context, constraints) {
        final effectiveWidth = constraints.maxWidth.isFinite
            ? math.min(width, constraints.maxWidth)
            : width;
        return SizedBox(
          width: effectiveWidth,
          child: TextField(
            controller: controller,
            obscureText: obscureText,
            decoration: InputDecoration(labelText: label),
            onSubmitted: onSubmitted,
          ),
        );
      },
    );
  }
}

class SmallSelect extends StatelessWidget {
  const SmallSelect({
    required this.label,
    required this.value,
    required this.options,
    required this.onChanged,
    this.width = 180,
    super.key,
  });

  final String label;
  final String value;
  final List<String> options;
  final ValueChanged<String> onChanged;
  final double width;

  @override
  Widget build(BuildContext context) {
    return LayoutBuilder(
      builder: (context, constraints) {
        final effectiveWidth = constraints.maxWidth.isFinite
            ? math.min(width, constraints.maxWidth)
            : width;
        return SizedBox(
          width: effectiveWidth,
          child: DropdownButtonFormField<String>(
            initialValue: options.contains(value) ? value : options.first,
            decoration: InputDecoration(labelText: label),
            items: options
                .map((item) => DropdownMenuItem(
                      value: item,
                      child: Text(item.isEmpty ? '全部' : item),
                    ))
                .toList(),
            onChanged: (value) {
              if (value != null) onChanged(value);
            },
          ),
        );
      },
    );
  }
}

class PagerBar extends StatelessWidget {
  const PagerBar({
    required this.page,
    required this.pageSize,
    required this.total,
    required this.onPageChanged,
    super.key,
  });

  final int page;
  final int pageSize;
  final int total;
  final ValueChanged<int> onPageChanged;

  @override
  Widget build(BuildContext context) {
    final maxPage = total <= 0 ? 1 : ((total + pageSize - 1) ~/ pageSize);
    return Wrap(
      spacing: 8,
      crossAxisAlignment: WrapCrossAlignment.center,
      children: [
        IconButton(
          tooltip: '上一页',
          onPressed: page <= 1 ? null : () => onPageChanged(page - 1),
          icon: const Icon(Icons.chevron_left),
        ),
        Text('第 $page / $maxPage 页，共 $total 条'),
        IconButton(
          tooltip: '下一页',
          onPressed: page >= maxPage ? null : () => onPageChanged(page + 1),
          icon: const Icon(Icons.chevron_right),
        ),
      ],
    );
  }
}
