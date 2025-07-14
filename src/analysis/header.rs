//! Parse the header dependency
//! For header files of a library, typically there are the dependency between them.
//! For example, libpng, the header file "pnglibconf.h" record the configuration of the library and "png.h" rely on it.
//! Such config-like files should be include by other headers and should not be included directly, otherwise the gadgets (apis, types and macros) parsed from headers files might be inconsisent with the built binary library.
//! The get_include_lib_headers() returns the top-level header files that a program should include.
//! The get_include_sys_headers() returns the system header files used in this library.

use crate::{config::get_library_name, deopt::Deopt, execution::Executor};
use eyre::Result;
use once_cell::sync::OnceCell;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::Path,
    process::{Command, Stdio},
};

use super::WorkList;

#[derive(Debug)]
struct TreeNode {
    name: String,
    children: Vec<TreeNode>,
}

impl TreeNode {
    fn new(name: String) -> Self {
        let name = name.replace("./", "");
        Self {
            name,
            children: Vec::new(),
        }
    }

    fn new_invalid() -> Self {
        Self {
            name: "invalid".to_string(),
            children: Vec::new(),
        }
    }

    fn is_invalid(&self) -> bool {
        self.name == "invalid"
    }

    fn set_name(&mut self, name: String) {
        self.name = name;
    }

    fn add_child(&mut self, child: TreeNode) {
        self.children.push(child)
    }

    fn get_name(&self) -> &str {
        &self.name
    }

    fn get_clean_root(&mut self, deopt: &Deopt) {
        let binding = deopt.get_library_build_header_path().unwrap();
        let include_path = binding.to_str().unwrap();
        let mut work_list = WorkList::new();
        work_list.push(self);
        while !work_list.empty() {
            let node = work_list.pop();
            let name = node.get_name();
            if name.starts_with(include_path) {
                let name = name.strip_prefix(include_path).unwrap();
                let name = name.strip_prefix('/').unwrap();
                node.set_name(name.to_string());
            }
            for child in &mut node.children {
                work_list.push(child);
            }
        }
    }
}

impl Executor {
    fn extract_header_dependency(&self, header: &Path) -> Result<TreeNode> {
        let header_path = self.deopt.get_library_build_header_path()?;
        let output = Command::new("clang++")
            .current_dir(&header_path)
            .arg("-fsyntax-only")
            .arg("-H")
            .arg("-I.")
            .arg(header)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .expect("fail to extract header dependency");
        if !output.status.success() {
            log::trace!("{}", String::from_utf8_lossy(&output.stderr));
            return Ok(TreeNode::new_invalid());
        }
        let base_name = header
            .to_str()
            .unwrap()
            .strip_prefix(header_path.to_str().unwrap())
            .unwrap();
        let base_name = [".", base_name].concat();
        let output = String::from_utf8_lossy(&output.stderr).to_string();
        let mut tree = parse_dependency_tree(&output, &base_name)?;
        tree.get_clean_root(&self.deopt);
        Ok(tree)
    }
}

fn get_layer_child(layered_nodes: Vec<(usize, &str)>, depth: usize) -> Vec<TreeNode> {
    let mut seq_each_layer = Vec::new();
    let mut layer_seqs = Vec::new();
    for (layer, node) in layered_nodes {
        if layer == depth {
            seq_each_layer.push(layer_seqs);
            layer_seqs = Vec::new();
        }
        layer_seqs.push((layer, node));
    }
    seq_each_layer.push(layer_seqs);
    seq_each_layer.retain(|x| !x.is_empty());

    let mut childs = Vec::new();
    for seq in seq_each_layer {
        let root = seq[0];
        let mut root = TreeNode::new(root.1.to_string());
        for child in get_layer_child(seq[1..].to_vec(), depth + 1) {
            root.add_child(child);
        }
        childs.push(root);
    }
    childs
}

fn parse_dependency_tree(output: &str, base_name: &str) -> Result<TreeNode> {
    let mut node_layer: Vec<(usize, &str)> = Vec::new();
    for line in output.lines() {
        let sep = line
            .find(' ')
            .ok_or_else(|| eyre::eyre!("Expect an spece in line: {line}"))?;
        let layer = sep;
        let header = line[sep..].trim();
        if header.contains("/usr/lib/") {
            continue;
        }
        if header.ends_with(".h") || header.ends_with(".hpp") || header.ends_with(".hxx") {
            node_layer.push((layer, header));
        }
    }
    let mut tree = TreeNode::new(base_name.to_owned());
    for child in get_layer_child(node_layer, 1) {
        tree.add_child(child);
    }
    Ok(tree)
}


fn get_independent_headers(trees: &[TreeNode]) -> Result<Vec<&str>> {
    
    // 构建依赖图：header -> 依赖它的headers
    let mut dependency_graph: HashMap<&str, HashSet<&str>> = HashMap::new();
    let mut all_headers: HashSet<&str> = HashSet::new();
    
    // 收集所有header名称
    for tree in trees {
        collect_all_headers(tree, &mut all_headers);
    }
    
    // 初始化依赖图
    for &header in &all_headers {
        dependency_graph.insert(header, HashSet::new());
    }
    
    // 构建依赖关系：如果header A包含header B，则B依赖A
    for tree in trees {
        build_dependency_graph(tree, tree.get_name(), &mut dependency_graph);
    }
    
    // 使用拓扑排序来找到顶层header，同时处理循环依赖
    find_top_level_headers_with_cycles(&dependency_graph, &all_headers)
}

fn collect_all_headers<'a>(tree: &'a TreeNode, all_headers: &mut HashSet<&'a str>) {
    let mut worklist = WorkList::new();
    worklist.push(tree);
    
    while !worklist.empty() {
        let node = worklist.pop();
        all_headers.insert(node.get_name());
        
        for child in &node.children {
            worklist.push(child);
        }
    }
}

fn build_dependency_graph<'a>(tree: &'a TreeNode, root_name: &'a str, dependency_graph: &mut HashMap<&'a str, HashSet<&'a str>>) {
    let mut worklist = WorkList::new();
    worklist.push(tree);
    
    while !worklist.empty() {
        let node = worklist.pop();
        
        // 对于每个子节点，表示它依赖当前根节点
        for child in &node.children {
            let child_name = child.get_name();
            if let Some(deps) = dependency_graph.get_mut(child_name) {
                deps.insert(root_name);
            }
            worklist.push(child);
        }
    }
}

fn find_top_level_headers_with_cycles<'a>(dependency_graph: &HashMap<&'a str, HashSet<&'a str>>, all_headers: &HashSet<&'a str>) -> Result<Vec<&'a str>> {
    
    // 计算每个header的入度（被多少个header依赖）
    let mut in_degree: HashMap<&str, usize> = HashMap::new();
    for &header in all_headers {
        in_degree.insert(header, 0);
    }
    
    for (_header, dependents) in dependency_graph {
        for &dependent in dependents {
            *in_degree.entry(dependent).or_insert(0) += 1;
        }
    }
    
    // 使用Kahn算法进行拓扑排序，找到入度为0的节点
    let mut queue = VecDeque::new();
    let mut result = Vec::new();
    let mut visited = HashSet::new();
    
    // 首先添加所有入度为0的节点
    for (&header, &degree) in &in_degree {
        if degree == 0 {
            queue.push_back(header);
        }
    }
    
    // 处理拓扑排序
    while let Some(header) = queue.pop_front() {
        if visited.contains(header) {
            continue;
        }
        visited.insert(header);
        result.push(header);
        
        // 减少依赖此header的其他header的入度
        if let Some(dependents) = dependency_graph.get(header) {
            for &dependent in dependents {
                if let Some(degree) = in_degree.get_mut(dependent) {
                    *degree -= 1;
                    if *degree == 0 && !visited.contains(dependent) {
                        queue.push_back(dependent);
                    }
                }
            }
        }
    }
    
    // 处理循环依赖：对于未访问的节点，选择字典序最小的作为代表
    let mut cycle_representatives = Vec::new();
    for &header in all_headers {
        if !visited.contains(header) {
            // 找到这个循环中字典序最小的header
            let mut cycle_nodes = Vec::new();
            let mut temp_visited = HashSet::new();
            find_cycle_nodes(header, dependency_graph, &mut cycle_nodes, &mut temp_visited);
            
            if !cycle_nodes.is_empty() {
                cycle_nodes.sort();
                let representative = cycle_nodes[0];
                if !visited.contains(representative) {
                    cycle_representatives.push(representative);
                    // 标记整个循环中的所有节点为已访问
                    for &node in &cycle_nodes {
                        visited.insert(node);
                    }
                }
            }
        }
    }
    
    // 合并结果
    result.extend(cycle_representatives);
    Ok(result)
}

fn find_cycle_nodes<'a>(start: &'a str, dependency_graph: &HashMap<&'a str, HashSet<&'a str>>, cycle_nodes: &mut Vec<&'a str>, temp_visited: &mut HashSet<&'a str>) {
    if temp_visited.contains(start) {
        return;
    }
    temp_visited.insert(start);
    cycle_nodes.push(start);
    
    if let Some(dependents) = dependency_graph.get(start) {
        for &dependent in dependents {
            find_cycle_nodes(dependent, dependency_graph, cycle_nodes, temp_visited);
        }
    }
}

fn is_a_lib_header(name: &str) -> bool {
    !name.starts_with('/')
}

fn get_included_sys_header(tree: &TreeNode) -> Vec<&str> {
    let mut worklist = WorkList::new();
    worklist.push(tree);
    let mut sys_headers = Vec::new();
    while !worklist.empty() {
        let node = worklist.pop();
        let name = node.get_name();
        // the names not start with '/' are lib headers
        if is_a_lib_header(name) {
            for child in &node.children {
                let child_name = child.get_name();
                if is_a_lib_header(child_name) {
                    continue;
                }
                sys_headers.push(child_name);
            }
        }
        for child in &node.children {
            worklist.push(child);
        }
    }
    sys_headers
}

fn get_library_dep_trees(deopt: &Deopt) -> &'static Vec<TreeNode> {
    static TREES: OnceCell<Vec<TreeNode>> = OnceCell::new();
    TREES.get_or_init(|| {
        let executor = Executor::new(deopt).unwrap();
        let header_dir = deopt.get_library_build_header_path().unwrap();
        let headers = crate::deopt::utils::read_all_files_in_dir(&header_dir).unwrap();
        let mut header_trees = Vec::new();
        for header in headers {
            let ext = header.extension();
            if ext.is_none() {
                continue;
            }
            let ext = ext.unwrap().to_string_lossy().to_string();
            if ext != "h" && ext != "hpp" && ext != "hxx" {
                continue;
            }
            let tree = executor.extract_header_dependency(&header).unwrap();
            if tree.is_invalid() {
                continue;
            }
            header_trees.push(tree);
        }
        header_trees
    })
}

pub fn get_include_lib_headers(deopt: &Deopt) -> Result<Vec<String>> {
    let header_trees = get_library_dep_trees(deopt);
    let header_strs: Vec<String> = get_independent_headers(header_trees)?
        .iter()
        .map(|x| x.to_string())
        .collect();
    Ok(header_strs)
}

pub fn get_include_sys_headers(deopt: &Deopt) -> &'static Vec<String> {
    static SYS_HEADERS: OnceCell<Vec<String>> = OnceCell::new();
    SYS_HEADERS.get_or_init(|| {
        let header_trees = get_library_dep_trees(deopt);
        let header_strs: Vec<String> = get_independent_headers(header_trees)
            .unwrap()
            .iter()
            .map(|x| x.to_string())
            .collect();
        let mut sys_headers = Vec::new();
        for tree in header_trees {
            let name = tree.get_name();
            if header_strs.contains(&name.to_string()) {
                let headers = get_included_sys_header(tree);
                // remove the prefix of sys headers
                for header in headers {
                    if let Some(idx) = header.rfind("/include/") {
                        let header = header[idx + "/include/".len()..].to_string();
                        if !sys_headers.contains(&header) {
                            sys_headers.push(header);
                        }
                    }
                }
            }
        }
        sys_headers
    })
}

pub fn get_include_sys_headers_str() -> String {
    let deopt = Deopt::new(get_library_name()).unwrap();
    let headers = get_include_sys_headers(&deopt);
    headers.join("\n")
}

#[test]
fn test_library_headers() {
    let deopt = Deopt::new("c-ares".to_string()).unwrap();
    let headers = get_include_lib_headers(&deopt).unwrap();
    let sys_headers = get_include_sys_headers(&deopt);
    assert_eq!(
        headers,
        vec!["aom/aomdx.h", "aom/aom_decoder.h", "aom/aomcx.h"]
    );
    assert_eq!(sys_headers, &vec!["stddef.h", "stdint.h", "inttypes.h"]);
}

#[test]
fn test_library_header() {
    crate::config::Config::init_test("libtiff");
    let deopt = Deopt::new("libtiff".to_string()).unwrap();
    let headers = get_include_lib_headers(&deopt).unwrap();
    println!("{headers:?}");
}
