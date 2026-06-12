import 'package:flutter/material.dart';
import 'package:file_selector/file_selector.dart';
import 'package:lucide_icons_flutter/lucide_icons.dart';

import '../core/widgets/stream_data_grid.dart';
import '../state.dart';
import '../utils.dart';
import '../widgets/app_select_field.dart';
import '../widgets/data_panel.dart';
import 'screen_helpers.dart';

class MediaUploadScreen extends StatefulWidget {
  const MediaUploadScreen({super.key});

  @override
  State<MediaUploadScreen> createState() => _MediaUploadScreenState();
}

class _MediaUploadScreenState extends State<MediaUploadScreen> {
  final fileController = TextEditingController();
  final keywordController = TextEditingController();
  final nodeController = TextEditingController();
  String statusFilter = 'active';
  int page = 1;
  final int pageSize = 50;
  int refreshSeed = 0;
  String? result;
  bool uploading = false;

  @override
  void dispose() {
    fileController.dispose();
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
        const PageHeader(
          title: '媒资上传',
          description:
              '通过 Rust native multipart 客户端上传本地文件到 StreamServer Agent 节点。',
        ),
        Surface(
          child: LayoutBuilder(
            builder: (context, constraints) {
              final narrow = constraints.maxWidth < 640;
              final pathField = TextField(
                controller: fileController,
                decoration: const InputDecoration(
                  labelText: '本地文件路径',
                  prefixIcon: Icon(Icons.insert_drive_file),
                ),
              );
              final actions = Wrap(
                spacing: 8,
                runSpacing: 8,
                children: [
                  OutlinedButton.icon(
                    onPressed: uploading ? null : _pickFile,
                    icon: const Icon(Icons.folder_open),
                    label: const Text('选择文件'),
                  ),
                  FilledButton.icon(
                    onPressed: uploading ? null : () => _upload(controller),
                    icon: uploading
                        ? const SizedBox(
                            width: 18,
                            height: 18,
                            child: CircularProgressIndicator(strokeWidth: 2))
                        : const Icon(Icons.upload_file),
                    label: Text(uploading ? '上传中' : '上传'),
                  ),
                ],
              );
              return narrow
                  ? Column(
                      crossAxisAlignment: CrossAxisAlignment.start,
                      children: [
                        pathField,
                        const SizedBox(height: 12),
                        actions,
                      ],
                    )
                  : Row(
                      children: [
                        Expanded(child: pathField),
                        const SizedBox(width: 12),
                        actions,
                      ],
                    );
            },
          ),
        ),
        if (result != null) ...[
          const SizedBox(height: 12),
          Surface(child: SelectableText(result!)),
        ],
        const SizedBox(height: 12),
        Surface(
          child: FilterBar(
            onApply: () => _refresh(resetPage: true),
            onReset: () {
              keywordController.clear();
              nodeController.clear();
              statusFilter = 'active';
              _refresh(resetPage: true);
            },
            children: [
              SmallSelect(
                label: '状态',
                value: statusFilter,
                options: const ['active', 'deleted', 'all'],
                onChanged: (value) => setState(() => statusFilter = value),
              ),
              SmallTextField(
                  controller: keywordController,
                  label: '关键字',
                  onSubmitted: (_) => _refresh(resetPage: true)),
              SmallTextField(
                  controller: nodeController,
                  label: '节点 ID',
                  onSubmitted: (_) => _refresh(resetPage: true)),
            ],
          ),
        ),
        const SizedBox(height: 12),
        AsyncDataPanel(
          key: ValueKey(refreshSeed),
          loader: (controller) => controller.api(
            'GET',
            '/api/v1/uploads/media',
            query: cleanQuery({
              'page': page,
              'page_size': pageSize,
              'status': statusFilter,
              'keyword': keywordController.text,
              'node_id': nodeController.text,
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
                      }),
                ),
                Surface(
                  child: LayoutBuilder(
                    builder: (context, constraints) {
                      if (constraints.maxWidth < 760) {
                        return _CompactUploadList(
                          rows: rows,
                          onDone: _refresh,
                        );
                      }
                      return StreamDataGrid(
                        height: 600,
                        rowHeight: 96,
                        rows: rows,
                        columns: [
                          StreamGridColumn(
                            title: '文件',
                            field: 'file_name',
                            width: 240,
                            renderer: (context, row, value) => gridTextCell(
                              context,
                              value,
                              fontWeight: FontWeight.w800,
                              maxWidth: 230,
                            ),
                          ),
                          StreamGridColumn(
                            title: '节点',
                            field: 'node_id',
                            width: 180,
                            renderer: (context, row, value) => gridTextCell(
                              context,
                              row['node_name'] ?? row['node_id'],
                              maxWidth: 170,
                            ),
                          ),
                          StreamGridColumn(
                            title: '大小',
                            field: 'file_size',
                            width: 110,
                            renderer: (context, row, value) =>
                                Text(bytesLabel(row['file_size'])),
                          ),
                          StreamGridColumn(
                            title: '时长',
                            field: 'duration_sec',
                            width: 90,
                            renderer: (context, row, value) =>
                                Text('${row['duration_sec'] ?? 0}s'),
                          ),
                          StreamGridColumn(
                            title: '状态',
                            field: 'status',
                            width: 120,
                            renderer: (context, row, value) =>
                                StatusBadge(status: value),
                          ),
                          StreamGridColumn(
                            title: 'HTTP 地址',
                            field: 'http_url',
                            width: 430,
                            renderer: (context, row, value) =>
                                FullUrlText(value: value, maxWidth: 410),
                          ),
                          StreamGridColumn(
                            title: '操作',
                            field: 'id',
                            width: 150,
                            renderer: (context, row, value) =>
                                _UploadIconActions(
                              row: row,
                              url: '${row['http_url'] ?? ''}',
                              onDone: _refresh,
                            ),
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

  Future<void> _pickFile() async {
    final file = await openFile();
    if (file == null) return;
    setState(() {
      fileController.text = file.path;
      result = '已选择：${file.name}';
    });
  }

  Future<void> _upload(AppController controller) async {
    final path = fileController.text.trim();
    if (path.isEmpty) {
      setState(() => result = '请先选择要上传的本地文件。');
      return;
    }
    setState(() {
      uploading = true;
      result = null;
    });
    try {
      final value = await controller.uploadMedia(path);
      setState(() {
        result = prettyJson(value);
        refreshSeed++;
      });
    } catch (cause) {
      setState(() => result = cause.toString());
    } finally {
      if (mounted) setState(() => uploading = false);
    }
  }
}

class _UploadIconActions extends StatelessWidget {
  const _UploadIconActions({
    required this.row,
    required this.url,
    required this.onDone,
  });

  final Map<String, Object?> row;
  final String url;
  final VoidCallback onDone;

  @override
  Widget build(BuildContext context) {
    final hasUrl = url.startsWith('http://') || url.startsWith('https://');
    final deleted = row['status'] == 'deleted' || row['file_deleted'] == true;
    final anchorController = MenuController();
    return Row(
      mainAxisSize: MainAxisSize.min,
      children: [
        IconButton(
          tooltip: '播放',
          onPressed: hasUrl ? () => _open(context) : null,
          icon: const Icon(LucideIcons.circlePlay, size: 17),
        ),
        IconButton(
          tooltip: '复制地址',
          onPressed: hasUrl ? () => copyText(context, url) : null,
          icon: const Icon(LucideIcons.copy, size: 17),
        ),
        MenuAnchor(
          controller: anchorController,
          alignmentOffset: const Offset(-128, 6),
          style: streamMenuStyle(context, minWidth: 156),
          menuChildren: [
            StreamMenuOption(
              width: 156,
              label: '删除台账',
              icon: LucideIcons.trash2,
              destructive: true,
              onPressed: deleted
                  ? null
                  : () {
                      anchorController.close();
                      _delete(context, false);
                    },
            ),
            StreamMenuOption(
              width: 156,
              label: '删除文件',
              icon: LucideIcons.trash,
              destructive: true,
              onPressed: deleted
                  ? null
                  : () {
                      anchorController.close();
                      _delete(context, true);
                    },
            ),
          ],
          builder: (context, menuController, child) {
            return Tooltip(
              message: '更多',
              waitDuration: const Duration(milliseconds: 450),
              child: IconButton(
                onPressed: deleted
                    ? null
                    : () {
                        if (menuController.isOpen) {
                          menuController.close();
                        } else {
                          menuController.open();
                        }
                      },
                icon: const Icon(LucideIcons.ellipsis, size: 18),
              ),
            );
          },
        ),
      ],
    );
  }

  Future<void> _open(BuildContext context) async {
    AppScope.of(context).playMedia(url, title: textValue(row['file_name']));
    showResult(context, '已打开内嵌播放器');
  }

  Future<void> _delete(BuildContext context, bool deleteFile) async {
    final name = textValue(row['file_name']);
    final confirmed = await confirmAction(
      context,
      title: deleteFile ? '删除上传文件' : '删除上传台账',
      message: deleteFile
          ? '确认删除 $name 的台账并同步删除底层文件？这可能影响历史任务和外部引用。'
          : '确认仅删除 $name 的上传台账？底层文件会保留。',
      confirmLabel: deleteFile ? '删文件' : '删台账',
      destructive: true,
    );
    if (!confirmed || !context.mounted) return;
    final controller = AppScope.of(context);
    try {
      await controller.api(
        'DELETE',
        '/api/v1/uploads/media/${row['id']}',
        query: {'delete_file': deleteFile},
      );
      if (context.mounted) {
        showResult(context, '删除请求已提交', tone: InlineStatusTone.success);
      }
      onDone();
    } catch (cause) {
      if (context.mounted) {
        showResult(context, cause.toString(), tone: InlineStatusTone.danger);
      }
    }
  }
}

class _CompactUploadList extends StatelessWidget {
  const _CompactUploadList({required this.rows, required this.onDone});

  final List<Map<String, Object?>> rows;
  final VoidCallback onDone;

  @override
  Widget build(BuildContext context) {
    if (rows.isEmpty) {
      return const SizedBox(
        height: 110,
        child: Center(child: Text('暂无上传记录')),
      );
    }
    return Column(
      children: [
        for (var index = 0; index < rows.length; index++) ...[
          _CompactUploadItem(row: rows[index], onDone: onDone),
          if (index != rows.length - 1)
            const Divider(height: 24, color: Color(0xffe4e8f0)),
        ],
      ],
    );
  }
}

class _CompactUploadItem extends StatelessWidget {
  const _CompactUploadItem({required this.row, required this.onDone});

  final Map<String, Object?> row;
  final VoidCallback onDone;

  @override
  Widget build(BuildContext context) {
    final url = '${row['http_url'] ?? ''}';
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Row(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Expanded(
              child: Text(
                textValue(row['file_name']),
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
            _UploadMeta(label: '节点', value: row['node_name'] ?? row['node_id']),
            _UploadMeta(label: '大小', value: bytesLabel(row['file_size'])),
            _UploadMeta(label: '时长', value: '${row['duration_sec'] ?? 0}s'),
          ],
        ),
        if (url.isNotEmpty) ...[
          const SizedBox(height: 8),
          SelectableText(url, style: const TextStyle(fontSize: 12)),
        ],
        const SizedBox(height: 8),
        _UploadActions(row: row, url: url, onDone: onDone),
      ],
    );
  }
}

class _UploadMeta extends StatelessWidget {
  const _UploadMeta({required this.label, required this.value});

  final String label;
  final Object? value;

  @override
  Widget build(BuildContext context) {
    return ConstrainedBox(
      constraints: const BoxConstraints(maxWidth: 280),
      child: RichText(
        text: metadataTextSpan(context, label: label, value: value),
        softWrap: true,
      ),
    );
  }
}

class _UploadActions extends StatelessWidget {
  const _UploadActions(
      {required this.row, required this.url, required this.onDone});

  final Map<String, Object?> row;
  final String url;
  final VoidCallback onDone;

  @override
  Widget build(BuildContext context) {
    final hasUrl = url.startsWith('http://') || url.startsWith('https://');
    final deleted = row['status'] == 'deleted' || row['file_deleted'] == true;
    return Wrap(
      spacing: 4,
      runSpacing: 4,
      children: [
        TextButton.icon(
          onPressed: hasUrl ? () => _open(context) : null,
          icon: const Icon(Icons.play_arrow),
          label: const Text('打开'),
        ),
        IconButton(
          tooltip: '复制地址',
          onPressed: hasUrl ? () => copyText(context, url) : null,
          icon: const Icon(Icons.copy),
        ),
        TextButton(
          onPressed: deleted ? null : () => _delete(context, deleteFile: false),
          child: const Text('删台账'),
        ),
        TextButton(
          onPressed: deleted ? null : () => _delete(context, deleteFile: true),
          child: const Text('删文件'),
        ),
      ],
    );
  }

  Future<void> _open(BuildContext context) async {
    AppScope.of(context).playMedia(url, title: textValue(row['file_name']));
    showResult(context, '已打开内嵌播放器');
  }

  Future<void> _delete(BuildContext context, {required bool deleteFile}) async {
    final name = textValue(row['file_name']);
    final confirmed = await confirmAction(
      context,
      title: deleteFile ? '删除上传文件' : '删除上传台账',
      message: deleteFile
          ? '确认删除 $name 的台账并同步删除底层文件？这可能影响历史任务和外部引用。'
          : '确认仅删除 $name 的上传台账？底层文件会保留。',
      confirmLabel: deleteFile ? '删文件' : '删台账',
      destructive: true,
    );
    if (!confirmed) return;
    if (!context.mounted) return;
    final controller = AppScope.of(context);
    try {
      await controller.api(
        'DELETE',
        '/api/v1/uploads/media/${row['id']}',
        query: {'delete_file': deleteFile},
      );
      if (context.mounted) {
        showResult(context, '删除请求已提交', tone: InlineStatusTone.success);
      }
      onDone();
    } catch (cause) {
      if (context.mounted) {
        showResult(context, cause.toString(), tone: InlineStatusTone.danger);
      }
    }
  }
}
