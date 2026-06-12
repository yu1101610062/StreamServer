import 'package:flutter/material.dart';
import 'package:lucide_icons_flutter/lucide_icons.dart';

import '../core/widgets/stream_data_grid.dart';
import '../state.dart';
import '../utils.dart';
import '../widgets/data_panel.dart';
import 'screen_helpers.dart';

class TasksScreen extends StatefulWidget {
  const TasksScreen({super.key});

  @override
  State<TasksScreen> createState() => _TasksScreenState();
}

class _TasksScreenState extends State<TasksScreen> {
  final keywordController = TextEditingController();
  final nodeController = TextEditingController();
  String statusFilter = '';
  String typeFilter = '';
  int page = 1;
  final int pageSize = 50;
  int refreshSeed = 0;

  @override
  void dispose() {
    keywordController.dispose();
    nodeController.dispose();
    super.dispose();
  }

  void _refresh({bool resetPage = false}) {
    setState(() {
      if (resetPage) page = 1;
      refreshSeed++;
    });
  }

  @override
  Widget build(BuildContext context) {
    final controller = AppScope.of(context);
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        PageHeader(
          title: '任务中心',
          description: '查看任务状态，执行启动、停止、取消、重试、克隆和删除等控制操作。',
          actions: FilledButton.icon(
            onPressed: () => controller.navigate(AppSection.taskCreate),
            icon: const Icon(Icons.add),
            label: const Text('新建任务'),
          ),
        ),
        Surface(
          child: FilterBar(
            onApply: () => _refresh(resetPage: true),
            onReset: () {
              keywordController.clear();
              nodeController.clear();
              statusFilter = '';
              typeFilter = '';
              _refresh(resetPage: true);
            },
            children: [
              SmallTextField(
                controller: keywordController,
                label: '关键字',
                width: 260,
                onSubmitted: (_) => _refresh(resetPage: true),
              ),
              SmallSelect(
                label: '状态',
                value: statusFilter,
                options: const [
                  '',
                  'CREATED',
                  'VALIDATING',
                  'QUEUED',
                  'STARTING',
                  'RUNNING',
                  'STOPPING',
                  'RECOVERING',
                  'SUCCEEDED',
                  'FAILED',
                  'LOST',
                  'CANCELED',
                ],
                onChanged: (value) => setState(() => statusFilter = value),
              ),
              SmallSelect(
                label: '类型',
                value: typeFilter,
                options: const [
                  '',
                  'stream_ingest',
                  'stream_bridge',
                  'file_transcode'
                ],
                onChanged: (value) => setState(() => typeFilter = value),
                width: 210,
              ),
              SmallTextField(
                controller: nodeController,
                label: '节点 ID',
                width: 220,
                onSubmitted: (_) => _refresh(resetPage: true),
              ),
            ],
          ),
        ),
        const SizedBox(height: 12),
        AsyncDataPanel(
          key: ValueKey(refreshSeed),
          loader: (controller) => controller.api(
            'GET',
            '/api/v1/tasks',
            query: cleanQuery({
              'page': page,
              'page_size': pageSize,
              'keyword': keywordController.text,
              'status': statusFilter,
              'type': typeFilter,
              'node_id': nodeController.text,
              'sort_by': 'created_at',
              'sort_order': 'desc',
            }),
          ),
          builder: (context, data) {
            final payload = (data as Map).cast<String, Object?>();
            final rows = rowsFrom(payload['items']);
            final total = (payload['total'] as num?)?.toInt() ?? rows.length;
            return Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Align(
                  alignment: Alignment.centerRight,
                  child: PagerBar(
                    page: page,
                    pageSize: pageSize,
                    total: total,
                    onPageChanged: (value) {
                      page = value;
                      _refresh();
                    },
                  ),
                ),
                Surface(
                  child: LayoutBuilder(
                    builder: (context, constraints) {
                      if (constraints.maxWidth < 820) {
                        return _CompactTaskList(
                          rows: rows,
                          onDone: _refresh,
                        );
                      }
                      return StreamDataGrid(
                        height: 560,
                        rows: rows,
                        onSelected: (row) =>
                            controller.openTask('${row['id']}'),
                        onDoubleTap: (row) =>
                            controller.openTask('${row['id']}'),
                        onSecondaryTap: (row) =>
                            _showTaskContextMenu(context, row, _refresh),
                        columns: [
                          StreamGridColumn(
                            title: '任务',
                            field: 'name',
                            width: 260,
                            enableRowChecked: true,
                            renderer: (context, row, value) => gridTextCell(
                              context,
                              value,
                              fontWeight: FontWeight.w800,
                              maxWidth: 248,
                            ),
                          ),
                          StreamGridColumn(
                            title: '类型',
                            field: 'type',
                            width: 150,
                            renderer: (context, row, value) =>
                                gridTextCell(context, value, maxWidth: 140),
                          ),
                          StreamGridColumn(
                            title: '状态',
                            field: 'status',
                            width: 130,
                            renderer: (context, row, value) =>
                                gridStatusCell(context, value),
                          ),
                          StreamGridColumn(
                            title: '节点',
                            field: 'assigned_node_id',
                            width: 110,
                            renderer: (context, row, value) => Text(shortId(
                                row['assigned_node_id'] ?? row['node_id'])),
                          ),
                          const StreamGridColumn(
                            title: '创建者',
                            field: 'created_by',
                            width: 120,
                          ),
                          const StreamGridColumn(
                            title: '更新时间',
                            field: 'updated_at',
                            width: 210,
                          ),
                          StreamGridColumn(
                            title: '操作',
                            field: 'id',
                            width: 128,
                            renderer: (context, row, value) =>
                                _TaskActionMenu(row: row, onDone: _refresh),
                          ),
                        ],
                      );
                    },
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

Future<void> _showTaskContextMenu(
  BuildContext context,
  Map<String, Object?> row,
  VoidCallback onDone,
) async {
  final overlay = Overlay.of(context).context.findRenderObject() as RenderBox?;
  final size = overlay?.size ?? const Size(1, 1);
  final selected = await showMenu<String>(
    context: context,
    position: RelativeRect.fromLTRB(size.width / 2, size.height / 2, 24, 24),
    items: const [
      PopupMenuItem(value: 'detail', child: Text('打开详情')),
      PopupMenuItem(value: 'start', child: Text('启动')),
      PopupMenuItem(value: 'stop', child: Text('停止')),
      PopupMenuItem(value: 'cancel', child: Text('取消')),
      PopupMenuItem(value: 'retry', child: Text('重试')),
      PopupMenuItem(value: 'clone', child: Text('克隆')),
      PopupMenuItem(value: 'delete', child: Text('删除')),
    ],
  );
  if (selected == null || !context.mounted) return;
  await _TaskActionMenu(row: row, onDone: onDone).run(context, selected);
}

class _CompactTaskList extends StatelessWidget {
  const _CompactTaskList({required this.rows, required this.onDone});

  final List<Map<String, Object?>> rows;
  final VoidCallback onDone;

  @override
  Widget build(BuildContext context) {
    if (rows.isEmpty) {
      return const SizedBox(
        height: 120,
        child: Center(child: Text('暂无任务')),
      );
    }
    return Column(
      children: [
        for (var index = 0; index < rows.length; index++) ...[
          _CompactTaskItem(row: rows[index], onDone: onDone),
          if (index != rows.length - 1)
            const Divider(height: 24, color: Color(0xffe4e8f0)),
        ],
      ],
    );
  }
}

class _CompactTaskItem extends StatelessWidget {
  const _CompactTaskItem({required this.row, required this.onDone});

  final Map<String, Object?> row;
  final VoidCallback onDone;

  @override
  Widget build(BuildContext context) {
    final controller = AppScope.of(context);
    final id = '${row['id']}';
    return InkWell(
      borderRadius: BorderRadius.circular(8),
      onTap: () => controller.openTask(id),
      child: Padding(
        padding: const EdgeInsets.symmetric(vertical: 4),
        child: Column(
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
                _TaskMeta(label: '类型', value: row['type']),
                _TaskMeta(
                  label: '节点',
                  value: shortId(row['assigned_node_id'] ?? row['node_id']),
                ),
                _TaskMeta(label: '创建者', value: row['created_by']),
                _TaskMeta(label: '更新时间', value: row['updated_at']),
              ],
            ),
            const SizedBox(height: 8),
            _TaskActions(row: row, onDone: onDone),
          ],
        ),
      ),
    );
  }
}

class _TaskMeta extends StatelessWidget {
  const _TaskMeta({required this.label, required this.value});

  final String label;
  final Object? value;

  @override
  Widget build(BuildContext context) {
    return ConstrainedBox(
      constraints: const BoxConstraints(maxWidth: 260),
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

class _TaskActions extends StatelessWidget {
  const _TaskActions({required this.row, required this.onDone});

  final Map<String, Object?> row;
  final VoidCallback onDone;

  @override
  Widget build(BuildContext context) {
    final controller = AppScope.of(context);
    final id = '${row['id']}';
    final name = textValue(row['name']);
    return Wrap(
      spacing: 4,
      runSpacing: 4,
      children: [
        TextButton(
            onPressed: () => controller.openTask(id), child: const Text('详情')),
        TextButton(
            onPressed: () =>
                _mutate(context, controller, 'POST', '/api/v1/tasks/$id/start'),
            child: const Text('启动')),
        TextButton(
          onPressed: () => _confirmMutate(
            context,
            controller,
            'POST',
            '/api/v1/tasks/$id/stop',
            '停止任务',
            '确认停止任务 $name？',
          ),
          child: const Text('停止'),
        ),
        TextButton(
          onPressed: () => _confirmMutate(
            context,
            controller,
            'POST',
            '/api/v1/tasks/$id/cancel',
            '取消任务',
            '确认取消任务 $name？',
          ),
          child: const Text('取消'),
        ),
        TextButton(
            onPressed: () =>
                _mutate(context, controller, 'POST', '/api/v1/tasks/$id/retry'),
            child: const Text('重试')),
        TextButton(
            onPressed: () =>
                _mutate(context, controller, 'POST', '/api/v1/tasks/$id/clone'),
            child: const Text('克隆')),
        TextButton(
          onPressed: () => _confirmMutate(
            context,
            controller,
            'DELETE',
            '/api/v1/tasks/$id',
            '删除任务',
            '确认删除任务 $name？该操作不可撤销。',
            destructive: true,
          ),
          child: const Text('删除'),
        ),
      ],
    );
  }

  Future<void> _confirmMutate(
    BuildContext context,
    AppController controller,
    String method,
    String path,
    String title,
    String message, {
    bool destructive = false,
  }) async {
    final confirmed = await confirmAction(
      context,
      title: title,
      message: message,
      destructive: destructive,
      confirmLabel: title.replaceAll('任务', ''),
    );
    if (!confirmed) return;
    if (!context.mounted) return;
    await _mutate(context, controller, method, path);
  }

  Future<void> _mutate(
    BuildContext context,
    AppController controller,
    String method,
    String path,
  ) async {
    try {
      await controller.mutate(method, path);
      if (context.mounted) showResult(context, '操作已提交');
      onDone();
    } catch (cause) {
      if (context.mounted) showResult(context, cause.toString());
    }
  }
}

class _TaskActionMenu extends StatelessWidget {
  const _TaskActionMenu({required this.row, required this.onDone});

  final Map<String, Object?> row;
  final VoidCallback onDone;

  @override
  Widget build(BuildContext context) {
    return Row(
      mainAxisSize: MainAxisSize.min,
      children: [
        IconButton(
          tooltip: '详情',
          onPressed: () => AppScope.of(context).openTask('${row['id']}'),
          icon: const Icon(LucideIcons.eye, size: 17),
        ),
        PopupMenuButton<String>(
          tooltip: '更多操作',
          onSelected: (value) => run(context, value),
          itemBuilder: (context) => const [
            PopupMenuItem(value: 'start', child: Text('启动')),
            PopupMenuItem(value: 'stop', child: Text('停止')),
            PopupMenuItem(value: 'cancel', child: Text('取消')),
            PopupMenuItem(value: 'retry', child: Text('重试')),
            PopupMenuItem(value: 'clone', child: Text('克隆')),
            PopupMenuDivider(),
            PopupMenuItem(value: 'delete', child: Text('删除')),
          ],
          child: const Padding(
            padding: EdgeInsets.all(8),
            child: Icon(LucideIcons.ellipsis, size: 18),
          ),
        ),
      ],
    );
  }

  Future<void> run(BuildContext context, String value) async {
    final controller = AppScope.of(context);
    final id = '${row['id']}';
    final name = textValue(row['name']);
    if (value == 'detail') {
      controller.openTask(id);
      return;
    }
    final path = '/api/v1/tasks/$id';
    String method = 'POST';
    String requestPath = path;
    bool confirm = false;
    bool destructive = false;
    String title = '';
    switch (value) {
      case 'start':
        requestPath = '$path/start';
        title = '启动任务';
        break;
      case 'stop':
        requestPath = '$path/stop';
        title = '停止任务';
        confirm = true;
        break;
      case 'cancel':
        requestPath = '$path/cancel';
        title = '取消任务';
        confirm = true;
        break;
      case 'retry':
        requestPath = '$path/retry';
        title = '重试任务';
        break;
      case 'clone':
        requestPath = '$path/clone';
        title = '克隆任务';
        break;
      case 'delete':
        method = 'DELETE';
        title = '删除任务';
        confirm = true;
        destructive = true;
        break;
      default:
        return;
    }
    if (confirm) {
      final ok = await confirmAction(
        context,
        title: title,
        message: destructive ? '确认删除任务 $name？该操作不可撤销。' : '确认$title $name？',
        confirmLabel: title.replaceAll('任务', ''),
        destructive: destructive,
      );
      if (!ok || !context.mounted) return;
    }
    try {
      await controller.mutate(method, requestPath);
      if (context.mounted) showResult(context, '操作已提交');
      onDone();
    } catch (cause) {
      if (context.mounted) showResult(context, cause.toString());
    }
  }
}
