import 'dart:math' as math;

import 'package:flutter/material.dart';

import '../utils.dart';
import '../widgets/data_panel.dart';

class OverviewScreen extends StatelessWidget {
  const OverviewScreen({super.key});

  @override
  Widget build(BuildContext context) {
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        const PageHeader(
          title: '系统总览',
          description: '在一页里查看任务、在线流、录像、文件产物和节点健康概况。',
        ),
        AsyncDataPanel(
          loader: (controller) async {
            final tasks = await controller
                .api('GET', '/api/v1/tasks', query: {'page_size': 8});
            final streams = await controller.api('GET', '/api/v1/streams');
            final records = await controller
                .api('GET', '/api/v1/records', query: {'page_size': 1});
            final artifacts = await controller
                .api('GET', '/api/v1/file-artifacts', query: {'page_size': 1});
            final nodes = await controller.api('GET', '/api/v1/nodes');
            return {
              'tasks': tasks,
              'streams': streams['value'],
              'records': records,
              'artifacts': artifacts,
              'nodes': nodes['value'],
            };
          },
          builder: (context, data) {
            final map = (data as Map).cast<String, Object?>();
            final taskPage = (map['tasks'] as Map).cast<String, Object?>();
            final streams = rowsFrom(map['streams']);
            final nodes = rowsFrom(map['nodes']);
            final records = (map['records'] as Map).cast<String, Object?>();
            final artifacts = (map['artifacts'] as Map).cast<String, Object?>();
            return Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                KeyValueGrid(
                  items: {
                    '任务总数': taskPage['total'],
                    '在线流': streams.length,
                    '录像记录': records['total'],
                    '文件产物': artifacts['total'],
                    '在线节点':
                        nodes.where((node) => node['healthy'] == true).length,
                  },
                ),
                const SizedBox(height: 18),
                Surface(
                  child: Column(
                    crossAxisAlignment: CrossAxisAlignment.start,
                    children: [
                      const Text('最近任务',
                          style: TextStyle(fontWeight: FontWeight.w700)),
                      const SizedBox(height: 12),
                      _RecentTasksTable(rowsFrom(taskPage['items'])),
                    ],
                  ),
                ),
              ],
            );
          },
        ),
      ],
    );
  }
}

class _RecentTasksTable extends StatelessWidget {
  const _RecentTasksTable(this.rows);

  final List<Map<String, Object?>> rows;

  @override
  Widget build(BuildContext context) {
    return LayoutBuilder(
      builder: (context, constraints) {
        if (constraints.maxWidth < 700) {
          return _RecentTasksList(rows);
        }
        final taskWidth =
            math.max(180.0, math.min(460.0, constraints.maxWidth * 0.42));
        return SingleChildScrollView(
          scrollDirection: Axis.horizontal,
          child: DataTable(
            dataRowMinHeight: 56,
            dataRowMaxHeight: 128,
            columns: const [
              DataColumn(label: Text('任务')),
              DataColumn(label: Text('类型')),
              DataColumn(label: Text('状态')),
              DataColumn(label: Text('创建时间')),
            ],
            rows: rows.map((row) {
              return DataRow(
                cells: [
                  DataCell(WrappedTextCell(
                      value: row['name'],
                      maxWidth: taskWidth,
                      fontWeight: FontWeight.w600)),
                  DataCell(WrappedTextCell(value: row['type'], maxWidth: 150)),
                  DataCell(StatusBadge(status: row['status'])),
                  DataCell(
                      WrappedTextCell(value: row['created_at'], maxWidth: 220)),
                ],
              );
            }).toList(),
          ),
        );
      },
    );
  }
}

class _RecentTasksList extends StatelessWidget {
  const _RecentTasksList(this.rows);

  final List<Map<String, Object?>> rows;

  @override
  Widget build(BuildContext context) {
    if (rows.isEmpty) {
      return const SizedBox(
        height: 96,
        child: Center(child: Text('暂无任务')),
      );
    }
    return Column(
      children: [
        for (var index = 0; index < rows.length; index++) ...[
          _RecentTaskItem(rows[index]),
          if (index != rows.length - 1)
            const Divider(height: 24, color: Color(0xffe4e8f0)),
        ],
      ],
    );
  }
}

class _RecentTaskItem extends StatelessWidget {
  const _RecentTaskItem(this.row);

  final Map<String, Object?> row;

  @override
  Widget build(BuildContext context) {
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Row(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Expanded(
              child: Text(
                textValue(row['name']),
                softWrap: true,
                style: const TextStyle(fontWeight: FontWeight.w700),
              ),
            ),
            const SizedBox(width: 10),
            StatusBadge(status: row['status']),
          ],
        ),
        const SizedBox(height: 10),
        Wrap(
          spacing: 14,
          runSpacing: 8,
          children: [
            _RecentTaskMeta(label: '类型', value: row['type']),
            _RecentTaskMeta(label: '创建时间', value: row['created_at']),
          ],
        ),
      ],
    );
  }
}

class _RecentTaskMeta extends StatelessWidget {
  const _RecentTaskMeta({required this.label, required this.value});

  final String label;
  final Object? value;

  @override
  Widget build(BuildContext context) {
    return ConstrainedBox(
      constraints: const BoxConstraints(maxWidth: 300),
      child: RichText(
        text: TextSpan(
          style: const TextStyle(color: Color(0xff1d2433), fontSize: 13),
          children: [
            TextSpan(
              text: '$label：',
              style: const TextStyle(
                color: Color(0xff5b6477),
                fontWeight: FontWeight.w600,
              ),
            ),
            TextSpan(text: textValue(value)),
          ],
        ),
        softWrap: true,
      ),
    );
  }
}
