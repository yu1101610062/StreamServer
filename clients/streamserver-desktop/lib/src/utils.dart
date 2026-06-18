import 'dart:convert';

List<Map<String, Object?>> rowsFrom(Object? value) {
  if (value is List) {
    return value
        .whereType<Map>()
        .map((item) => item.cast<String, Object?>())
        .toList();
  }
  if (value is Map) {
    final items = value['items'];
    if (items is List) {
      return items
          .whereType<Map>()
          .map((item) => item.cast<String, Object?>())
          .toList();
    }
    final raw = value['value'];
    if (raw is List) {
      return raw
          .whereType<Map>()
          .map((item) => item.cast<String, Object?>())
          .toList();
    }
  }
  return const [];
}

String prettyJson(Object? value) {
  const encoder = JsonEncoder.withIndent('  ');
  return encoder.convert(value);
}

String shortId(Object? value) {
  final text = '${value ?? ''}';
  if (text.length <= 12) return text;
  return text.substring(0, 8);
}

String textValue(Object? value) {
  if (value == null) return '—';
  if (value is String && value.isEmpty) return '—';
  return '$value';
}

String bytesLabel(Object? value) {
  final bytes =
      value is num ? value.toDouble() : double.tryParse('$value') ?? 0;
  if (bytes >= 1024 * 1024 * 1024) {
    return '${(bytes / 1024 / 1024 / 1024).toStringAsFixed(2)} GB';
  }
  if (bytes >= 1024 * 1024) {
    return '${(bytes / 1024 / 1024).toStringAsFixed(2)} MB';
  }
  if (bytes >= 1024) return '${(bytes / 1024).toStringAsFixed(1)} KB';
  return '${bytes.toStringAsFixed(0)} B';
}

String runtimeSlotLoadsLabel(Object? value) {
  if (value is! Iterable) return '—';
  final labels = <String>[];
  for (final item in value) {
    if (item is! Map) continue;
    final sourceMode = '${item['source_mode'] ?? ''}';
    final modeLabel = switch (sourceMode) {
      'live' => '直播',
      'vod' => '点播',
      _ => sourceMode.isEmpty ? '未知' : sourceMode,
    };
    final maxSlots = item['max_runtime_slots'] is num
        ? item['max_runtime_slots'] as num
        : num.tryParse('${item['max_runtime_slots']}') ?? 0;
    num count(Object? raw) => raw is num ? raw : num.tryParse('$raw') ?? 0;
    final occupied = count(item['running_tasks']) +
        count(item['starting_tasks']) +
        count(item['stopping_tasks']) +
        count(item['orphaned_tasks']);
    final usage = count(item['slot_usage']) * 100;
    final maxLabel = maxSlots == 0 ? '不限' : maxSlots.toStringAsFixed(0);
    labels.add(
        '$modeLabel ${occupied.toStringAsFixed(0)}/$maxLabel ${usage.toStringAsFixed(1)}%');
  }
  return labels.isEmpty ? '—' : labels.join(' · ');
}

String pathQuery(Map<String, Object?> query) {
  final params = query.entries
      .where((entry) => entry.value != null && '${entry.value}'.isNotEmpty)
      .map((entry) =>
          '${entry.key}=${Uri.encodeQueryComponent('${entry.value}')}');
  final joined = params.join('&');
  return joined.isEmpty ? '' : '?$joined';
}
