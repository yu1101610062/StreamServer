import 'dart:math' as math;

import 'package:fl_chart/fl_chart.dart';
import 'package:flutter/material.dart';
import 'package:lucide_icons_flutter/lucide_icons.dart';

import '../core/theme/stream_theme.dart';
import '../state.dart';
import '../utils.dart';
import '../widgets/data_panel.dart';

class OverviewScreen extends StatelessWidget {
  const OverviewScreen({super.key});

  @override
  Widget build(BuildContext context) {
    return AsyncDataPanel(
      loader: (controller) async {
        final results = await Future.wait<Map<String, Object?>>([
          _loadOverviewTaskSample(controller),
          _loadTaskStatusTotals(controller),
          controller.api('GET', '/api/v1/streams'),
          controller.api('GET', '/api/v1/records',
              query: const {'page_size': 1}),
          controller.api('GET', '/api/v1/file-artifacts',
              query: const {'page_size': 1}),
          controller.api('GET', '/api/v1/nodes'),
          controller.api('GET', '/api/v1/uploads/media',
              query: const {'page_size': 1}),
        ]);
        final tasks = results[0];
        final taskStatusTotals = results[1];
        final streams = results[2];
        final records = results[3];
        final artifacts = results[4];
        final nodes = results[5];
        final uploads = results[6];
        return {
          'tasks': tasks,
          'task_status_totals': taskStatusTotals,
          'streams': streams['value'],
          'records': records,
          'artifacts': artifacts,
          'nodes': nodes['value'],
          'uploads': uploads,
        };
      },
      builder: (context, data) {
        final map = (data as Map).cast<String, Object?>();
        final taskPage = (map['tasks'] as Map).cast<String, Object?>();
        final taskStatusTotals =
            (map['task_status_totals'] as Map).cast<String, Object?>();
        final tasks = rowsFrom(taskPage['items']);
        final nodes = rowsFrom(map['nodes']);
        final records = (map['records'] as Map).cast<String, Object?>();
        final artifacts = (map['artifacts'] as Map).cast<String, Object?>();
        final uploads = (map['uploads'] as Map).cast<String, Object?>();
        final totalTasks = (taskPage['total'] as num?)?.toInt() ?? tasks.length;
        final uploadTotal = (uploads['total'] as num?)?.toInt() ?? 0;
        final healthyNodes =
            nodes.where((node) => node['healthy'] == true).length;
        final runningTasks = _sumStatusTotals(
            taskStatusTotals, const ['RUNNING', 'STARTING', 'RECOVERING']);
        final failedTasks =
            _sumStatusTotals(taskStatusTotals, const ['FAILED', 'LOST']);
        final statusEntries =
            _buildStatusEntries(taskStatusTotals, totalTasks);
        return LayoutBuilder(
          builder: (context, constraints) {
            final wide = constraints.maxWidth >= 1180;
            final medium = constraints.maxWidth >= 760;
            final content = Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                _MetricGrid(
                  medium: medium,
                  cards: [
                    _MetricCard(
                      title: '运行任务',
                      value: '$runningTasks',
                      note: '任务总数 $totalTasks',
                      icon: LucideIcons.clipboardList,
                      color: context.streamColors.primary,
                      values: _spark(totalTasks, 0),
                    ),
                    _MetricCard(
                      title: '健康节点',
                      value: '$healthyNodes',
                      note:
                          '在线率 ${nodes.isEmpty ? 0 : (healthyNodes / nodes.length * 100).round()}%',
                      icon: LucideIcons.server,
                      color: context.streamColors.success,
                      values: _spark(healthyNodes, 1),
                    ),
                    _MetricCard(
                      title: '录像总记录',
                      value: '${records['total'] ?? 0}',
                      note: '文件产物 ${artifacts['total'] ?? 0}',
                      icon: LucideIcons.hardDrive,
                      color: context.streamColors.orange,
                      values:
                          _spark((records['total'] as num?)?.toInt() ?? 0, 2),
                      progress: 0.62,
                    ),
                    _MetricCard(
                      title: '上传队列',
                      value: '${uploads['total'] ?? 0}',
                      note: '上传台账',
                      icon: LucideIcons.upload,
                      color: context.streamColors.purple,
                      values:
                          _spark((uploads['total'] as num?)?.toInt() ?? 0, 3),
                    ),
                    _MetricCard(
                      title: '失败任务',
                      value: '$failedTasks',
                      note: failedTasks == 0 ? '当前无失败' : '需要处理',
                      icon: LucideIcons.flame,
                      color: context.streamColors.danger,
                      values: _spark(failedTasks, 4),
                    ),
                  ],
                ),
                const SizedBox(height: 18),
              ],
            );
            final leftContent = Column(
              children: [
                if (wide)
                  Row(
                    crossAxisAlignment: CrossAxisAlignment.start,
                    children: [
                      Expanded(
                        child: _StatusDistribution(
                          entries: statusEntries,
                          total: totalTasks,
                        ),
                      ),
                      const SizedBox(width: 16),
                      Expanded(child: _TaskTrend(tasks: tasks)),
                    ],
                  )
                else
                  Column(
                    children: [
                      _StatusDistribution(
                        entries: statusEntries,
                        total: totalTasks,
                      ),
                      const SizedBox(height: 16),
                      _TaskTrend(tasks: tasks),
                    ],
                  ),
                const SizedBox(height: 18),
                _RecentTasksTable(rows: tasks),
              ],
            );
            if (!wide) {
              return Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  content,
                  leftContent,
                ],
              );
            }
            return Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                content,
                Row(
                  crossAxisAlignment: CrossAxisAlignment.start,
                  children: [
                    Expanded(child: leftContent),
                    const SizedBox(width: 16),
                    SizedBox(
                      width: 330,
                      child: _OverviewInspector(
                        nodes: nodes,
                        totalTasks: totalTasks,
                        healthyNodes: healthyNodes,
                        failedTasks: failedTasks,
                        uploadTotal: uploadTotal,
                      ),
                    ),
                  ],
                ),
              ],
            );
          },
        );
      },
    );
  }
}

class _MetricGrid extends StatelessWidget {
  const _MetricGrid({required this.medium, required this.cards});

  final bool medium;
  final List<_MetricCard> cards;

  @override
  Widget build(BuildContext context) {
    return LayoutBuilder(
      builder: (context, constraints) {
        final columns = constraints.maxWidth >= 1120
            ? 5
            : constraints.maxWidth >= 880
                ? 3
                : medium
                    ? 2
                    : 1;
        final width = (constraints.maxWidth - 16 * (columns - 1)) / columns;
        return Wrap(
          spacing: 16,
          runSpacing: 16,
          children: [
            for (final card in cards) SizedBox(width: width, child: card),
          ],
        );
      },
    );
  }
}

class _OverviewInspector extends StatelessWidget {
  const _OverviewInspector({
    required this.nodes,
    required this.totalTasks,
    required this.healthyNodes,
    required this.failedTasks,
    required this.uploadTotal,
  });

  final List<Map<String, Object?>> nodes;
  final int totalTasks;
  final int healthyNodes;
  final int failedTasks;
  final int uploadTotal;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    final controller = AppScope.of(context);
    final node = nodes.isEmpty ? null : nodes.first;
    final healthy = node?['healthy'] == true;
    return Surface(
      padding: EdgeInsets.zero,
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Padding(
            padding: const EdgeInsets.fromLTRB(18, 16, 14, 14),
            child: Row(
              children: [
                Expanded(
                  child: Text(
                    '节点详情',
                    style: TextStyle(
                      color: colors.textPrimary,
                      fontWeight: FontWeight.w800,
                    ),
                  ),
                ),
                const Icon(LucideIcons.x, size: 18),
              ],
            ),
          ),
          Divider(height: 1, color: colors.border),
          Padding(
            padding: const EdgeInsets.all(18),
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                if (node == null)
                  const SizedBox(height: 80, child: Center(child: Text('暂无节点')))
                else ...[
                  Row(
                    children: [
                      Container(
                        width: 10,
                        height: 10,
                        decoration: BoxDecoration(
                          color: healthy ? colors.success : colors.danger,
                          shape: BoxShape.circle,
                          boxShadow: [
                            BoxShadow(
                              color: (healthy ? colors.success : colors.danger)
                                  .withValues(alpha: 0.35),
                              blurRadius: 10,
                            ),
                          ],
                        ),
                      ),
                      const SizedBox(width: 10),
                      Expanded(
                        child: Text(
                          textValue(node['node_name'] ??
                              node['hostname'] ??
                              node['id']),
                          style: TextStyle(
                            color: colors.textPrimary,
                            fontSize: 20,
                            fontWeight: FontWeight.w900,
                          ),
                        ),
                      ),
                      StatusBadge(status: healthy ? 'healthy' : 'unhealthy'),
                    ],
                  ),
                  const SizedBox(height: 14),
                  _OptionalInspectorLine('节点 ID', node['id']),
                  _OptionalInspectorLine('主机名', node['hostname']),
                  _OptionalInspectorLine('标签', node['labels']),
                  const SizedBox(height: 18),
                  GridView.count(
                    crossAxisCount: 2,
                    shrinkWrap: true,
                    physics: const NeverScrollableScrollPhysics(),
                    mainAxisSpacing: 10,
                    crossAxisSpacing: 10,
                    childAspectRatio: 1.45,
                    children: [
                      _ResourceTile('任务总数', '$totalTasks', colors.primary),
                      _ResourceTile('健康节点', '$healthyNodes', colors.success),
                      _ResourceTile('失败任务', '$failedTasks', colors.danger),
                      _ResourceTile('上传台账', '$uploadTotal', colors.purple),
                      if (_formatPercent(node['cpu_percent']) != null)
                        _ResourceTile(
                            'CPU',
                            _formatPercent(node['cpu_percent'])!,
                            colors.primary),
                      if (_formatPercent(node['mem_percent']) != null)
                        _ResourceTile(
                            '内存',
                            _formatPercent(node['mem_percent'])!,
                            colors.purple),
                      if (_formatPercent(node['disk_percent']) != null)
                        _ResourceTile(
                            '磁盘',
                            _formatPercent(node['disk_percent'])!,
                            colors.orange),
                      if (_formatPlain(node['running_tasks']) != null)
                        _ResourceTile(
                            '运行任务',
                            _formatPlain(node['running_tasks'])!,
                            colors.success),
                    ],
                  ),
                  const SizedBox(height: 18),
                  _OverviewServiceStatus(node: node),
                  const SizedBox(height: 18),
                  _OverviewHeartbeatInfo(node: node),
                  const SizedBox(height: 16),
                  _InspectorLine('Core', controller.server?.baseUrl ?? '未连接'),
                ],
              ],
            ),
          ),
        ],
      ),
    );
  }
}

class _OptionalInspectorLine extends StatelessWidget {
  const _OptionalInspectorLine(this.label, this.value);

  final String label;
  final Object? value;

  @override
  Widget build(BuildContext context) {
    if (!_hasInspectorValue(value)) return const SizedBox.shrink();
    return _InspectorLine(label, _formatInspectorValue(value));
  }
}

class _OverviewServiceStatus extends StatelessWidget {
  const _OverviewServiceStatus({required this.node});

  final Map<String, Object?> node;

  @override
  Widget build(BuildContext context) {
    final rows = [
      _OverviewBoolSpec('控制连接', node['control_connected']),
      _OverviewBoolSpec('媒体服务', node['media_alive']),
      _OverviewBoolSpec('ZLM', node['zlm_alive']),
      _OverviewBoolSpec('FFmpeg', node['ffmpeg_alive']),
    ].where((item) => item.value is bool).toList();
    if (rows.isEmpty) return const SizedBox.shrink();
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Text(
          '服务状态',
          style: TextStyle(
            color: context.streamColors.textPrimary,
            fontWeight: FontWeight.w800,
          ),
        ),
        const SizedBox(height: 10),
        for (final row in rows) _ServiceRow(row.label, row.value == true),
      ],
    );
  }
}

class _OverviewHeartbeatInfo extends StatelessWidget {
  const _OverviewHeartbeatInfo({required this.node});

  final Map<String, Object?> node;

  @override
  Widget build(BuildContext context) {
    final rows = [
      MapEntry('最后心跳', node['last_seen_at']),
      MapEntry('控制心跳', node['control_last_seen_at']),
      MapEntry('媒体心跳', node['media_last_seen_at']),
    ].where((entry) => _hasInspectorValue(entry.value)).toList();
    if (rows.isEmpty) return const SizedBox.shrink();
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Text(
          '心跳信息',
          style: TextStyle(
            color: context.streamColors.textPrimary,
            fontWeight: FontWeight.w800,
          ),
        ),
        const SizedBox(height: 10),
        for (final row in rows)
          _InspectorLine(row.key, _formatInspectorValue(row.value)),
      ],
    );
  }
}

class _OverviewBoolSpec {
  const _OverviewBoolSpec(this.label, this.value);

  final String label;
  final Object? value;
}

class _InspectorLine extends StatelessWidget {
  const _InspectorLine(this.label, this.value);

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
            width: 72,
            child: Text(
              label,
              style: TextStyle(color: colors.textSecondary, fontSize: 12),
            ),
          ),
          Expanded(
            child: Text(
              value,
              style: TextStyle(
                color: colors.textPrimary,
                fontSize: 12,
                fontWeight: FontWeight.w700,
              ),
            ),
          ),
        ],
      ),
    );
  }
}

class _ResourceTile extends StatelessWidget {
  const _ResourceTile(this.label, this.value, this.color);

  final String label;
  final String value;
  final Color color;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    return Container(
      padding: const EdgeInsets.all(12),
      decoration: BoxDecoration(
        color: alpha(colors.surfaceAlt, context.isDarkMode ? 0.74 : 0.9),
        border: Border.all(color: colors.border),
        borderRadius: BorderRadius.circular(8),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        mainAxisAlignment: MainAxisAlignment.spaceBetween,
        children: [
          Text(
            label,
            style: TextStyle(color: colors.textSecondary, fontSize: 12),
          ),
          Row(
            children: [
              Expanded(
                child: Text(
                  value,
                  maxLines: 1,
                  overflow: TextOverflow.ellipsis,
                  style: TextStyle(
                    color: colors.textPrimary,
                    fontSize: 20,
                    fontWeight: FontWeight.w900,
                  ),
                ),
              ),
              Icon(LucideIcons.activity, color: color, size: 16),
            ],
          ),
        ],
      ),
    );
  }
}

class _ServiceRow extends StatelessWidget {
  const _ServiceRow(this.name, this.ok);

  final String name;
  final bool ok;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    return Padding(
      padding: const EdgeInsets.symmetric(vertical: 6),
      child: Row(
        children: [
          Icon(LucideIcons.circle,
              size: 10, color: ok ? colors.success : colors.danger),
          const SizedBox(width: 8),
          Expanded(
            child: Text(name, style: TextStyle(color: colors.textSecondary)),
          ),
          StatusBadge(status: ok ? 'running' : 'offline'),
        ],
      ),
    );
  }
}

String? _formatPercent(Object? value) {
  if (!_hasInspectorValue(value)) return null;
  final number = value is num ? value : num.tryParse('$value');
  if (number == null) return textValue(value);
  return '${number.toStringAsFixed(number % 1 == 0 ? 0 : 1)}%';
}

String? _formatPlain(Object? value) {
  if (!_hasInspectorValue(value)) return null;
  return _formatInspectorValue(value);
}

bool _hasInspectorValue(Object? value) {
  if (value == null) return false;
  if (value is String) return value.trim().isNotEmpty;
  if (value is Iterable) return value.isNotEmpty;
  if (value is Map) return value.isNotEmpty;
  return true;
}

String _formatInspectorValue(Object? value) {
  if (value is Iterable) {
    return value
        .map((item) => '$item')
        .where((item) => item.isNotEmpty)
        .join(', ');
  }
  if (value is Map) return prettyJson(value);
  return textValue(value);
}

class _MetricCard extends StatelessWidget {
  const _MetricCard({
    required this.title,
    required this.value,
    required this.note,
    required this.icon,
    required this.color,
    required this.values,
    this.progress,
  });

  final String title;
  final String value;
  final String note;
  final IconData icon;
  final Color color;
  final List<double> values;
  final double? progress;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    return Surface(
      padding: const EdgeInsets.all(18),
      child: SizedBox(
        height: 136,
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Row(
              children: [
                Container(
                  width: 38,
                  height: 38,
                  decoration: BoxDecoration(
                    color: color.withValues(alpha: 0.13),
                    shape: BoxShape.circle,
                  ),
                  child: Icon(icon, color: color, size: 18),
                ),
                const SizedBox(width: 12),
                Expanded(
                  child: Text(
                    title,
                    style: TextStyle(color: colors.textSecondary, fontSize: 13),
                  ),
                ),
              ],
            ),
            const SizedBox(height: 9),
            Text(
              value,
              maxLines: 1,
              overflow: TextOverflow.ellipsis,
              style: TextStyle(
                color: colors.textPrimary,
                fontSize: 26,
                fontWeight: FontWeight.w800,
                height: 1,
              ),
            ),
            const SizedBox(height: 6),
            Text(note,
                style: TextStyle(color: colors.textSecondary, fontSize: 12)),
            const Spacer(),
            if (progress != null)
              ClipRRect(
                borderRadius: BorderRadius.circular(999),
                child: LinearProgressIndicator(
                  minHeight: 7,
                  value: progress,
                  backgroundColor: colors.border,
                  valueColor: AlwaysStoppedAnimation(color),
                ),
              )
            else
              SizedBox(
                  height: 28, child: _Sparkline(values: values, color: color)),
          ],
        ),
      ),
    );
  }
}

class _Sparkline extends StatelessWidget {
  const _Sparkline({required this.values, required this.color});

  final List<double> values;
  final Color color;

  @override
  Widget build(BuildContext context) {
    return LineChart(
      LineChartData(
        minY: 0,
        gridData: const FlGridData(show: false),
        titlesData: const FlTitlesData(show: false),
        borderData: FlBorderData(show: false),
        lineBarsData: [
          LineChartBarData(
            isCurved: true,
            color: color,
            dotData: const FlDotData(show: false),
            belowBarData: BarAreaData(
              show: true,
              color: color.withValues(alpha: 0.11),
            ),
            spots: [
              for (var i = 0; i < values.length; i++)
                FlSpot(i.toDouble(), values[i]),
            ],
          ),
        ],
      ),
    );
  }
}

class _StatusDistribution extends StatelessWidget {
  const _StatusDistribution({
    required this.entries,
    required this.total,
  });

  final List<_StatusCount> entries;
  final int total;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    final denominator = math.max(1, total);
    final visibleEntries = entries.isEmpty
        ? [const _StatusCount(status: 'UNKNOWN', count: 0)]
        : entries;
    return Surface(
      padding: const EdgeInsets.all(18),
      child: LayoutBuilder(
        builder: (context, constraints) {
          final narrow = constraints.maxWidth < 680;
          final chart = SizedBox(
            width: narrow ? 210 : 220,
            height: 220,
            child: PieChart(
              PieChartData(
                centerSpaceRadius: 50,
                sectionsSpace: 2,
                sections: [
                  for (var index = 0; index < visibleEntries.length; index++)
                    _pie(
                      visibleEntries[index].count,
                      denominator,
                      _statusColor(visibleEntries[index].status, colors, index),
                    ),
                ],
              ),
            ),
          );
          final legend = _StatusLegendGrid(
            entries: visibleEntries,
            total: denominator,
          );
          return ConstrainedBox(
            constraints: const BoxConstraints(minHeight: 270),
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Text('任务状态分布',
                    style: TextStyle(
                        color: colors.textPrimary,
                        fontWeight: FontWeight.w800)),
                const SizedBox(height: 14),
                if (narrow)
                  Column(
                    children: [
                      Center(child: chart),
                      const SizedBox(height: 14),
                      legend,
                    ],
                  )
                else
                  Row(
                    crossAxisAlignment: CrossAxisAlignment.center,
                    children: [
                      chart,
                      const SizedBox(width: 26),
                      Expanded(child: legend),
                    ],
                  ),
              ],
            ),
          );
        },
      ),
    );
  }

  PieChartSectionData _pie(int value, int total, Color color) {
    return PieChartSectionData(
      value: value <= 0 ? 0.01 : value.toDouble(),
      title: '',
      color: color,
      radius: 58,
    );
  }
}

class _StatusLegendGrid extends StatelessWidget {
  const _StatusLegendGrid({required this.entries, required this.total});

  final List<_StatusCount> entries;
  final int total;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    return LayoutBuilder(
      builder: (context, constraints) {
        final columns = constraints.maxWidth >= 720
            ? 3
            : constraints.maxWidth >= 430
                ? 2
                : 1;
        final itemWidth = (constraints.maxWidth - 16 * (columns - 1)) / columns;
        return Wrap(
          spacing: 16,
          runSpacing: 10,
          children: [
            for (var index = 0; index < entries.length; index++)
              SizedBox(
                width: itemWidth,
                child: _LegendRow(
                  entries[index],
                  total,
                  _statusColor(entries[index].status, colors, index),
                ),
              ),
          ],
        );
      },
    );
  }
}

class _LegendRow extends StatelessWidget {
  const _LegendRow(this.entry, this.total, this.color);

  final _StatusCount entry;
  final int total;
  final Color color;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    return Container(
      padding: const EdgeInsets.symmetric(horizontal: 10, vertical: 8),
      decoration: BoxDecoration(
        color: colors.surfaceAlt
            .withValues(alpha: context.isDarkMode ? 0.42 : 0.8),
        border: Border.all(color: colors.border),
        borderRadius: BorderRadius.circular(8),
      ),
      child: Row(
        children: [
          Container(
            width: 8,
            height: 8,
            decoration: BoxDecoration(color: color, shape: BoxShape.circle),
          ),
          const SizedBox(width: 10),
          Expanded(
            child: Text(
              _statusDisplayName(entry.status),
              maxLines: 1,
              overflow: TextOverflow.ellipsis,
              style: TextStyle(
                color: colors.textPrimary,
                fontSize: 12,
                fontWeight: FontWeight.w700,
              ),
            ),
          ),
          const SizedBox(width: 8),
          Text(
            '${entry.count} (${(entry.count / total * 100).toStringAsFixed(1)}%)',
            style: TextStyle(
              color: colors.textSecondary,
              fontSize: 12,
              fontWeight: FontWeight.w700,
            ),
          ),
        ],
      ),
    );
  }
}

class _StatusCount {
  const _StatusCount({required this.status, required this.count});

  final String status;
  final int count;
}

class _TaskTrend extends StatelessWidget {
  const _TaskTrend({required this.tasks});

  final List<Map<String, Object?>> tasks;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    final trend = _buildTrendSeries(tasks);
    final runningValues = trend.running;
    final successValues = trend.success;
    final failedValues = trend.failed;
    final maxValue = [
      ...runningValues,
      ...successValues,
      ...failedValues,
    ].fold<double>(0, math.max);
    final maxY = math.max(5.0, (maxValue / 5).ceil() * 5.0);
    final interval = math.max(1.0, maxY / 5);
    return Surface(
      padding: const EdgeInsets.all(18),
      child: LayoutBuilder(
        builder: (context, constraints) {
          final compact = constraints.maxWidth < 460;
          return SizedBox(
            height: compact ? 320 : 300,
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                if (compact)
                  Column(
                    crossAxisAlignment: CrossAxisAlignment.start,
                    children: [
                      Text(
                        '任务趋势（近 7 天）',
                        maxLines: 1,
                        overflow: TextOverflow.ellipsis,
                        style: TextStyle(
                          color: colors.textPrimary,
                          fontWeight: FontWeight.w800,
                        ),
                      ),
                      const SizedBox(height: 8),
                      _TrendLegend(colors: colors),
                    ],
                  )
                else
                  Row(
                    children: [
                      Expanded(
                        child: Text(
                          '任务趋势（近 7 天）',
                          maxLines: 1,
                          overflow: TextOverflow.ellipsis,
                          style: TextStyle(
                            color: colors.textPrimary,
                            fontWeight: FontWeight.w800,
                          ),
                        ),
                      ),
                      _TrendLegend(colors: colors),
                    ],
                  ),
                const SizedBox(height: 12),
                Expanded(
                  child: LineChart(
                    LineChartData(
                      minY: 0,
                      maxY: maxY,
                      minX: 0,
                      maxX: 6,
                      gridData: FlGridData(
                        drawVerticalLine: false,
                        getDrawingHorizontalLine: (_) =>
                            FlLine(color: colors.border, strokeWidth: 1),
                      ),
                      titlesData: FlTitlesData(
                        topTitles: const AxisTitles(
                            sideTitles: SideTitles(showTitles: false)),
                        rightTitles: const AxisTitles(
                            sideTitles: SideTitles(showTitles: false)),
                        leftTitles: AxisTitles(
                          sideTitles: SideTitles(
                            showTitles: true,
                            reservedSize: 48,
                            interval: interval,
                            getTitlesWidget: (value, meta) {
                              if (value < 0 ||
                                  value > maxY ||
                                  value % interval > 0.001) {
                                return const SizedBox.shrink();
                              }
                              return Padding(
                                padding: const EdgeInsets.only(right: 10),
                                child: Text(
                                  value.toInt().toString(),
                                  textAlign: TextAlign.right,
                                  style: TextStyle(
                                    color: colors.textSecondary,
                                    fontSize: 11,
                                    height: 1,
                                  ),
                                ),
                              );
                            },
                          ),
                        ),
                        bottomTitles: AxisTitles(
                          sideTitles: SideTitles(
                            showTitles: true,
                            reservedSize: 28,
                            interval: 1,
                            getTitlesWidget: (value, meta) {
                              final index = value.toInt();
                              if (index < 0 ||
                                  index >= trend.labels.length ||
                                  value != index.toDouble()) {
                                return const SizedBox.shrink();
                              }
                              return Padding(
                                padding: const EdgeInsets.only(top: 8),
                                child: Text(
                                  trend.labels[index],
                                  style: TextStyle(
                                    color: colors.textSecondary,
                                    fontSize: compact ? 10 : 11,
                                  ),
                                ),
                              );
                            },
                          ),
                        ),
                      ),
                      borderData: FlBorderData(show: false),
                      lineBarsData: [
                        _trendBar(runningValues, colors.primary),
                        _trendBar(successValues, colors.success),
                        _trendBar(failedValues, colors.danger),
                      ],
                    ),
                  ),
                ),
              ],
            ),
          );
        },
      ),
    );
  }

  LineChartBarData _trendBar(List<double> values, Color color) {
    return LineChartBarData(
      isCurved: true,
      color: color,
      dotData: const FlDotData(show: true),
      barWidth: 2,
      belowBarData:
          BarAreaData(show: true, color: color.withValues(alpha: 0.07)),
      spots: [
        for (var i = 0; i < values.length; i++) FlSpot(i.toDouble(), values[i]),
      ],
    );
  }
}

class _TrendLegend extends StatelessWidget {
  const _TrendLegend({required this.colors});

  final StreamColors colors;

  @override
  Widget build(BuildContext context) {
    return Wrap(
      spacing: 12,
      runSpacing: 6,
      children: [
        _TrendLegendItem('运行中', colors.primary),
        _TrendLegendItem('完成', colors.success),
        _TrendLegendItem('失败', colors.danger),
      ],
    );
  }
}

class _TrendLegendItem extends StatelessWidget {
  const _TrendLegendItem(this.label, this.color);

  final String label;
  final Color color;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    return Row(
      mainAxisSize: MainAxisSize.min,
      children: [
        Container(
          width: 7,
          height: 7,
          decoration: BoxDecoration(color: color, shape: BoxShape.circle),
        ),
        const SizedBox(width: 6),
        Text(
          label,
          style: TextStyle(color: colors.textSecondary, fontSize: 12),
        ),
      ],
    );
  }
}

class _RecentTasksTable extends StatelessWidget {
  const _RecentTasksTable({required this.rows});

  final List<Map<String, Object?>> rows;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    return Surface(
      padding: const EdgeInsets.all(18),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text('最近任务',
              style: TextStyle(
                  color: colors.textPrimary, fontWeight: FontWeight.w800)),
          const SizedBox(height: 14),
          if (rows.isEmpty)
            const SizedBox(height: 90, child: Center(child: Text('暂无任务')))
          else
            LayoutBuilder(
              builder: (context, constraints) {
                if (constraints.maxWidth < 720) return _RecentTaskCards(rows);
                return SizedBox(
                  width: double.infinity,
                  child: DataTable(
                    columnSpacing: math.max(24, constraints.maxWidth * 0.045),
                    horizontalMargin: 18,
                    headingRowHeight: 40,
                    dataRowMinHeight: 52,
                    dataRowMaxHeight: 76,
                    columns: const [
                      DataColumn(label: Text('任务 ID')),
                      DataColumn(label: Text('类型')),
                      DataColumn(label: Text('节点')),
                      DataColumn(label: Text('状态')),
                      DataColumn(label: Text('开始时间')),
                      DataColumn(label: Text('操作')),
                    ],
                    rows: rows.take(6).map((row) {
                      return DataRow(
                        cells: [
                          DataCell(WrappedTextCell(
                              value: row['name'] ?? row['id'],
                              maxWidth: constraints.maxWidth * 0.28,
                              fontWeight: FontWeight.w700)),
                          DataCell(Text(textValue(row['type']))),
                          DataCell(Text(shortId(
                              row['assigned_node_id'] ?? row['node_id']))),
                          DataCell(StatusBadge(status: row['status'])),
                          DataCell(Text(textValue(
                              row['created_at'] ?? row['updated_at']))),
                          const DataCell(Icon(LucideIcons.ellipsis, size: 18)),
                        ],
                      );
                    }).toList(),
                  ),
                );
              },
            ),
        ],
      ),
    );
  }
}

class _RecentTaskCards extends StatelessWidget {
  const _RecentTaskCards(this.rows);

  final List<Map<String, Object?>> rows;

  @override
  Widget build(BuildContext context) {
    return Column(
      children: [
        for (final row in rows.take(8))
          Container(
            margin: const EdgeInsets.only(bottom: 10),
            padding: const EdgeInsets.all(12),
            decoration: BoxDecoration(
              color: context.streamColors.surfaceAlt,
              border: Border.all(color: context.streamColors.border),
              borderRadius: BorderRadius.circular(8),
            ),
            child: Row(
              children: [
                Expanded(
                  child: Text(
                    textValue(row['name'] ?? row['id']),
                    style: const TextStyle(fontWeight: FontWeight.w800),
                  ),
                ),
                StatusBadge(status: row['status']),
              ],
            ),
          ),
      ],
    );
  }
}

Future<Map<String, Object?>> _loadOverviewTaskSample(
  AppController controller,
) async {
  const pageSize = 100;
  final first = await controller.api(
    'GET',
    '/api/v1/tasks',
    query: const {
      'page': 1,
      'page_size': pageSize,
      'sort_by': 'created_at',
      'sort_order': 'desc',
    },
  );
  final firstPage = (first as Map).cast<String, Object?>();
  final firstItems = rowsFrom(firstPage['items']);
  final total = (firstPage['total'] as num?)?.toInt() ?? firstItems.length;
  return {
    ...firstPage,
    'items': firstItems,
    'total': total,
    'sample_limit': pageSize,
  };
}

Future<Map<String, Object?>> _loadTaskStatusTotals(
  AppController controller,
) async {
  final entries = await Future.wait([
    for (final status in _overviewStatusKeys)
      _loadTaskStatusTotal(controller, status),
  ]);
  return {
    for (final entry in entries) entry.key: entry.value,
  };
}

Future<MapEntry<String, int>> _loadTaskStatusTotal(
  AppController controller,
  String status,
) async {
  final page = await controller.api(
    'GET',
    '/api/v1/tasks',
    query: {
      'page': 1,
      'page_size': 1,
      'status': status,
    },
  );
  final total =
      ((page as Map).cast<String, Object?>()['total'] as num?)?.toInt() ?? 0;
  return MapEntry(status, total);
}

int _sumStatusTotals(Map<String, Object?> totals, List<String> statuses) {
  return statuses.fold<int>(0, (sum, status) {
    return sum + ((totals[status.toUpperCase()] as num?)?.toInt() ?? 0);
  });
}

List<_StatusCount> _buildStatusEntries(Map<String, Object?> totals, int total) {
  final counts = <String, int>{};
  for (final status in _overviewStatusKeys) {
    final count = (totals[status] as num?)?.toInt() ?? 0;
    if (count > 0) counts[status] = count;
  }
  final countedTotal = counts.values.fold<int>(0, (sum, value) => sum + value);
  if (total > countedTotal) {
    counts['UNKNOWN'] = total - countedTotal;
  }
  final entries = counts.entries
      .map((entry) => _StatusCount(status: entry.key, count: entry.value))
      .toList();
  entries.sort((left, right) {
    final leftRank = _statusSortRank(left.status);
    final rightRank = _statusSortRank(right.status);
    if (leftRank != rightRank) return leftRank.compareTo(rightRank);
    final byCount = right.count.compareTo(left.count);
    if (byCount != 0) return byCount;
    return left.status.compareTo(right.status);
  });
  return entries;
}

const _overviewStatusKeys = [
  'CREATED',
  'VALIDATING',
  'QUEUED',
  'STARTING',
  'RUNNING',
  'STOPPING',
  'RECOVERING',
  'SUCCEEDED',
  'COMPLETED',
  'FAILED',
  'LOST',
  'CANCELED',
  'CANCELLED',
];

int _statusSortRank(String status) {
  const order = [
    'CREATED',
    'VALIDATING',
    'QUEUED',
    'STARTING',
    'RUNNING',
    'STOPPING',
    'RECOVERING',
    'SUCCEEDED',
    'COMPLETED',
    'FAILED',
    'LOST',
    'CANCELED',
    'CANCELLED',
    'UNKNOWN',
    'UNLOADED',
  ];
  final index = order.indexOf(status.toUpperCase());
  return index < 0 ? order.length : index;
}

Color _statusColor(String status, StreamColors colors, int index) {
  switch (status.toUpperCase()) {
    case 'RUNNING':
      return colors.primary;
    case 'SUCCEEDED':
    case 'COMPLETED':
    case 'SUCCESS':
      return colors.success;
    case 'FAILED':
    case 'ERROR':
      return colors.danger;
    case 'LOST':
      return colors.warning;
    case 'CANCELED':
    case 'CANCELLED':
      return colors.textMuted;
    case 'CREATED':
      return colors.purple;
    case 'VALIDATING':
      return const Color(0xff06b6d4);
    case 'QUEUED':
      return const Color(0xff0ea5e9);
    case 'STARTING':
      return const Color(0xffa855f7);
    case 'STOPPING':
      return colors.orange;
    case 'RECOVERING':
      return const Color(0xffc084fc);
    default:
      final palette = [
        colors.purple,
        colors.orange,
        const Color(0xff14b8a6),
        const Color(0xffeab308),
        const Color(0xff64748b),
        const Color(0xffec4899),
      ];
      return palette[index % palette.length];
  }
}

String _statusDisplayName(String status) {
  switch (status.toUpperCase()) {
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
    case 'CREATED':
      return '已创建';
    case 'VALIDATING':
      return '校验中';
    case 'QUEUED':
      return '排队中';
    case 'STARTING':
      return '启动中';
    case 'STOPPING':
      return '停止中';
    case 'RECOVERING':
      return '恢复中';
    case 'UNKNOWN':
      return '未知';
    case 'UNLOADED':
      return '未载入';
    default:
      return status;
  }
}

_TrendSeries _buildTrendSeries(List<Map<String, Object?>> tasks) {
  final today = DateUtils.dateOnly(DateTime.now());
  final days = List.generate(
    7,
    (index) => today.subtract(Duration(days: 6 - index)),
  );
  final running = List<double>.filled(7, 0);
  final success = List<double>.filled(7, 0);
  final failed = List<double>.filled(7, 0);
  for (final task in tasks) {
    final createdAt = _parseTaskTime(task['created_at'] ?? task['updated_at']);
    if (createdAt == null) continue;
    final day = DateUtils.dateOnly(createdAt.toLocal());
    final index = days.indexWhere((candidate) => candidate == day);
    if (index < 0) continue;
    final status = textValue(task['status']).toUpperCase();
    if (const {'RUNNING', 'STARTING', 'RECOVERING'}.contains(status)) {
      running[index] += 1;
    } else if (const {'SUCCEEDED', 'COMPLETED', 'SUCCESS'}.contains(status)) {
      success[index] += 1;
    } else if (const {'FAILED', 'LOST', 'ERROR'}.contains(status)) {
      failed[index] += 1;
    }
  }
  return _TrendSeries(
    labels: [
      for (final day in days)
        '${day.month.toString().padLeft(2, '0')}-${day.day.toString().padLeft(2, '0')}',
    ],
    running: running,
    success: success,
    failed: failed,
  );
}

DateTime? _parseTaskTime(Object? value) {
  if (value == null) return null;
  if (value is DateTime) return value;
  return DateTime.tryParse('$value');
}

class _TrendSeries {
  const _TrendSeries({
    required this.labels,
    required this.running,
    required this.success,
    required this.failed,
  });

  final List<String> labels;
  final List<double> running;
  final List<double> success;
  final List<double> failed;
}

List<double> _spark(int seed, int offset) {
  final base = math.max(4, seed + 6 + offset);
  return List.generate(12, (index) {
    final wave = math.sin((index + offset) * 0.9) * 4;
    final climb = index * (1.2 + offset * 0.08);
    return math.max(1, base + wave + climb);
  });
}
